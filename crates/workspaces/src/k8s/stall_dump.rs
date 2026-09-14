//! Where is a stalled kube request parked? A tokio task dump taken at the moment the inner bound
//! fires (`kube.timeout layer=inner`), logged as one `kube.stall.dump` line.
//!
//! 2026-09-14 (`k3s-stall` investigation): inner-layer stalls on a pooled HTTP/1 connection
//! (dials=0) answer in ms on retry and start just after the keys tick's fsync loop. The candidate
//! is a wake lost while the connection task sat in a blocked worker's LIFO slot; only the
//! runtime's own view of every task at the stall can confirm or kill that.
//!
//! Off unless `ClusterSettings.spec.stallDumps` is true (the agent stores it into `ENABLED` on
//! every settings apply), at most one dump per `MIN_GAP` per process. The dump itself needs
//! `RUSTFLAGS="--cfg tokio_unstable"` plus the `stall-dump` cargo feature on Linux; any other
//! build logs `kube.stall.dump.unsupported` instead, so a flag switched on against the wrong
//! image says so rather than staying silent. No pool snapshot: hyper-util's legacy client has no
//! public pool stats.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub static ENABLED: AtomicBool = AtomicBool::new(false);

pub const MIN_GAP: Duration = Duration::from_secs(600);
/// `Handle::dump` never resolves while another worker is blocked past 250 ms; a timeout here is
/// itself the evidence of a blocked worker.
pub const DUMP_TIMEOUT: Duration = Duration::from_secs(2);
pub const MAX_BYTES: usize = 64 * 1024;

/// Seconds since process start of the last claimed dump, +1 so 0 means "never".
static LAST: AtomicU64 = AtomicU64::new(0);

fn secs_now() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_secs() + 1
}

/// One winner per `MIN_GAP`, lock-free, so concurrent stalls cannot each pause the runtime.
fn claim(now: u64) -> bool {
    let last = LAST.load(Ordering::Relaxed);
    (last == 0 || now.saturating_sub(last) >= MIN_GAP.as_secs())
        && LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
}

/// The resource kind of a kube path — never a namespace, object name or owner handle.
pub fn resource_of(path: &str) -> &str {
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let rest = match segs.as_slice() {
        ["api", _v, rest @ ..] => rest,
        ["apis", _g, _v, rest @ ..] => rest,
        _ => return "unknown",
    };
    match rest {
        ["namespaces", _ns, kind, ..] => kind,
        [kind, ..] => kind,
        [] => "unknown",
    }
}

pub fn on_inner_timeout(method: &http::Method, path: &str) {
    if !ENABLED.load(Ordering::Relaxed) || !claim(secs_now()) {
        return;
    }
    let (method, resource) = (method.clone(), resource_of(path).to_string());
    #[cfg(all(tokio_unstable, feature = "stall-dump", target_os = "linux"))]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        // An OS thread, never a task: the bound and the log line must not need a runtime worker,
        // since a wedged worker is exactly what this is looking for.
        std::thread::spawn(move || match dump(handle, MAX_BYTES) {
            Some(d) => tracing::warn!(%method, %resource, tasks = d.tasks, matched = d.matched, dump_ms = d.ms, truncated = d.truncated, dump = %d.text, "kube.stall.dump"),
            None => tracing::warn!(%method, %resource, timeout_ms = DUMP_TIMEOUT.as_millis() as u64, "kube.stall.dump.timeout"),
        });
    }
    #[cfg(not(all(tokio_unstable, feature = "stall-dump", target_os = "linux")))]
    tracing::warn!(%method, %resource, "kube.stall.dump.unsupported");
}

#[cfg(all(tokio_unstable, feature = "stall-dump", target_os = "linux"))]
pub struct Dump {
    pub tasks: usize,
    pub matched: usize,
    pub ms: u64,
    pub truncated: bool,
    pub text: String,
}

/// A dump that never finished; a second one would spin in tokio's `start_trace_request` forever.
#[cfg(all(tokio_unstable, feature = "stall-dump", target_os = "linux"))]
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Blocks the CALLING OS thread for at most `DUMP_TIMEOUT` — call it from a plain thread, never a
/// runtime worker. The bound is `recv_timeout`, outside the runtime, because tokio's own can hang:
/// `trace_core` (tokio 1.53.1 `multi_thread/worker/taskdump.rs:21`) waits on a barrier with a
/// 250 ms `wait_timeout`, and a timed-out waiter returns WITHOUT taking back its `count += 1`
/// (`loom/std/barrier.rs`, `wait_timeout`), so a later worker becomes leader alone and then parks
/// in the untimed `trace_end.wait()` (`taskdump.rs:54`) forever — with every worker so parked the
/// timer driver never turns and a `tokio::time::timeout` around the dump never fires. On timeout
/// the dump thread is abandoned: it owns only its own current-thread runtime and a oneshot, so
/// dropping our receiver cancels nothing tokio relies on; whatever wedged stays wedged either way.
/// Tasks whose trace touches the HTTP stack only; the rest of the runtime is counted, not printed.
#[cfg(all(tokio_unstable, feature = "stall-dump", target_os = "linux"))]
pub fn dump(handle: tokio::runtime::Handle, max: usize) -> Option<Dump> {
    use std::fmt::Write as _;
    if IN_FLIGHT.swap(true, Ordering::Relaxed) {
        return None;
    }
    let start = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().build().expect("dump runtime");
        let snap = rt.block_on(handle.dump());
        IN_FLIGHT.store(false, Ordering::Relaxed);
        let _ = tx.send(snap);
    });
    let snap = rx.recv_timeout(DUMP_TIMEOUT).ok()?;
    let ms = start.elapsed().as_millis() as u64;
    let (mut tasks, mut matched, mut text) = (0, 0, String::new());
    for t in snap.tasks().iter() {
        tasks += 1;
        let trace = t.trace().to_string();
        if ["hyper", "kube", "tower"].iter().any(|k| trace.contains(k)) {
            matched += 1;
            let _ = writeln!(text, "task {}:\n{trace}", t.id());
        }
    }
    let truncated = text.len() > max;
    if truncated {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Some(Dump { tasks, matched, ms, truncated, text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_is_the_kind_never_a_name() {
        assert_eq!(resource_of("/api/v1/nodes/session-0"), "nodes");
        assert_eq!(resource_of("/apis/kloudlite.io/v1alpha1/ownerkeys/karthik1729/status"), "ownerkeys");
        assert_eq!(resource_of("/api/v1/namespaces/ws-karthik/pods/p"), "pods");
        assert_eq!(resource_of("/apis/kloudlite.io/v1alpha1/snapshots"), "snapshots");
        assert_eq!(resource_of("/version"), "unknown");
    }

    #[test]
    fn one_claim_per_gap() {
        assert!(claim(5));
        assert!(!claim(6));
        assert!(!claim(5 + MIN_GAP.as_secs() - 1));
        assert!(claim(5 + MIN_GAP.as_secs()));
    }

    /// Linux + `--cfg tokio_unstable` + `stall-dump` only, so it runs in the pod gate, never on a Mac.
    /// A plain `#[test]` over a two-worker runtime (the agent's shape) so the test thread is never
    /// a worker, and `shutdown_timeout` so a wedged worker fails the test instead of hanging it.
    #[cfg(all(tokio_unstable, feature = "stall-dump", target_os = "linux"))]
    #[test]
    fn dump_names_a_parked_http_task() {
        #[inline(never)]
        async fn hyper_parked_marker(n: std::sync::Arc<tokio::sync::Notify>) {
            n.notified().await;
        }
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let n = std::sync::Arc::new(tokio::sync::Notify::new());
        rt.spawn(hyper_parked_marker(n.clone()));
        std::thread::sleep(Duration::from_millis(50));
        let d = dump(rt.handle().clone(), MAX_BYTES);
        let tiny = d.as_ref().and_then(|_| dump(rt.handle().clone(), 16));
        n.notify_one();
        rt.shutdown_timeout(Duration::from_secs(1));
        let d = d.expect("no worker is blocked, so the dump must finish inside DUMP_TIMEOUT");
        eprintln!("dump_ms={} tasks={} matched={}", d.ms, d.tasks, d.matched);
        assert!(d.text.contains("hyper_parked_marker"), "{}", d.text);
        let tiny = tiny.expect("a second dump after a finished one");
        assert!(tiny.truncated && tiny.text.len() <= 16);
    }
}
