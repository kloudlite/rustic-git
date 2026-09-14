//! Where is a stalled kube request parked? A tokio task dump taken while an inner-layer read is
//! still waiting (`STALL_AT`, before its bound), logged as one `kube.stall.dump` line.
//!
//! Part 3 of the investigation: dumps taken AFTER the bound were useless — the stalled future was
//! already dropped, hyper had closed its connection and the retry had answered, and 64 KB of
//! repeated monomorphized names kept 16 of 37 tasks. So the dump fires mid-stall, generics are
//! stripped, identical traces print once with a count, and caller tasks lead.
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

/// Before the inner read bound (4.5 s), so the stalled request is still parked when we look.
pub const STALL_AT: Duration = Duration::from_millis(3500);

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

pub fn on_stall(method: &http::Method, path: &str, elapsed_ms: u64, newest_conn: u64) {
    if !ENABLED.load(Ordering::Relaxed) || !claim(secs_now()) {
        return;
    }
    let (method, resource) = (method.clone(), resource_of(path).to_string());
    // `conn_serial` is unobtainable (see `Counted`); `newest_conn` is the last dial's serial.
    #[cfg(all(tokio_unstable, feature = "stall-dump", target_os = "linux"))]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        // An OS thread, never a task: the bound and the log line must not need a runtime worker,
        // since a wedged worker is exactly what this is looking for.
        std::thread::spawn(move || match dump(handle, MAX_BYTES) {
            Some(d) => tracing::warn!(%method, %resource, elapsed_ms, newest_conn, conn_serial = "unknown", tasks = d.tasks, matched = d.matched, distinct = d.distinct, dump_ms = d.ms, truncated = d.truncated, dump = %d.text, "kube.stall.dump"),
            None => tracing::warn!(%method, %resource, elapsed_ms, newest_conn, timeout_ms = DUMP_TIMEOUT.as_millis() as u64, "kube.stall.dump.timeout"),
        });
    }
    #[cfg(not(all(tokio_unstable, feature = "stall-dump", target_os = "linux")))]
    tracing::warn!(%method, %resource, elapsed_ms, newest_conn, "kube.stall.dump.unsupported");
}

/// Past this, a generic argument list that is not a qualified path (`<X as Trait>`) prints `<…>`.
const MAX_GENERIC: usize = 40;

/// One frame as `name at file:line`. tokio's own `Display` for a frame drops the last `::`
/// segment (tokio 1.53.1 `task/trace/symbol.rs:69`), which turned `<X as Future>::poll` into
/// `<X as Future>` and, with generics stripped, into nothing — every decisive frame of the
/// 2026-09-14 in-stall dump printed as an empty line. So names come from the demangled symbol.
pub fn frame_line(demangled: &str, file: Option<&str>, line: Option<u32>) -> String {
    let name = match demangled.rsplit_once("::h") {
        Some((n, h)) if h.len() == 16 && h.bytes().all(|c| c.is_ascii_hexdigit()) => n,
        _ => demangled,
    };
    let mut out = shorten(name);
    if let Some(f) = file {
        let f = f.find("index.crates.io-").map_or(f, |ix| f[ix..].find('/').map_or(f, |i| &f[ix + i + 1..]));
        out.push_str(" at ");
        out.push_str(f);
        if let Some(l) = line {
            out.push_str(&format!(":{l}"));
        }
    }
    out
}

/// Index of the `>` closing the `<` at `open`; a `->` inside is not a close.
fn close_of(s: &str, open: usize) -> Option<usize> {
    let (b, mut depth) = (s.as_bytes(), 0usize);
    for i in open..b.len() {
        match b[i] {
            b'<' => depth += 1,
            b'>' if i == 0 || b[i - 1] != b'-' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Byte index of the last `pat` outside every `<…>`.
fn rfind_top(s: &str, pat: &str) -> Option<usize> {
    let (b, mut depth, mut hit) = (s.as_bytes(), 0usize, None);
    for i in 0..b.len() {
        match b[i] {
            b'<' => depth += 1,
            b'>' if depth > 0 && (i == 0 || b[i - 1] != b'-') => depth -= 1,
            _ if depth == 0 && s[i..].starts_with(pat) => hit = Some(i),
            _ => {}
        }
    }
    hit
}

/// The outer path stays whole; each `<…>` inside it is shortened by `generic`.
fn shorten(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let (mut i, mut run) = (0, 0);
    while let Some(off) = s[i..].find('<') {
        let open = i + off;
        let Some(end) = close_of(s, open) else { break };
        out.push_str(&s[run..open]);
        out.push('<');
        out.push_str(&generic(&s[open + 1..end]));
        out.push('>');
        i = end + 1;
        run = i;
    }
    out.push_str(&s[run..]);
    out
}

/// `X as Trait` keeps both type names' last path segment; a long plain list collapses to `…`.
fn generic(inner: &str) -> String {
    let last = |p: &str| shorten(rfind_top(p, "::").map_or(p, |i| &p[i + 2..]));
    match rfind_top(inner, " as ") {
        Some(i) => format!("{} as {}", last(&inner[..i]), last(&inner[i + 4..])),
        None if inner.len() > MAX_GENERIC => "…".to_string(),
        None => shorten(inner),
    }
}

/// `(task id, formatted trace)` in, one block per distinct trace out: callers (kube/tower/our
/// own frames) first, connection tasks after, each `count x tasks [ids]`. Returns (text, distinct).
pub fn render(tasks: &[(String, String)]) -> (String, usize) {
    use std::fmt::Write as _;
    let mut groups: Vec<(String, Vec<&str>)> = Vec::new();
    for (id, t) in tasks {
        match groups.iter_mut().find(|g| &g.0 == t) {
            Some(g) => g.1.push(id),
            None => groups.push((t.clone(), vec![id])),
        }
    }
    let caller = |t: &str| ["kube", "tower", "kloudlite"].iter().any(|k| t.contains(k));
    groups.sort_by_key(|g| !caller(&g.0));
    let mut text = String::new();
    for (t, ids) in &groups {
        let shown: Vec<&str> = ids.iter().take(8).copied().collect();
        let _ = writeln!(text, "{}x tasks [{}]:\n{t}", ids.len(), shown.join(","));
    }
    (text, groups.len())
}

#[cfg(all(tokio_unstable, feature = "stall-dump", target_os = "linux"))]
pub struct Dump {
    pub tasks: usize,
    pub matched: usize,
    pub distinct: usize,
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
    let mut tasks = 0;
    let mut hits = Vec::new();
    for t in snap.tasks().iter() {
        tasks += 1;
        let trace = t
            .trace()
            .resolve_backtraces()
            .iter()
            .map(|bt| {
                bt.frames()
                    .flat_map(|f| f.symbols())
                    .filter_map(|sym| {
                        let file = sym.filename().map(|p| p.to_string_lossy().into_owned());
                        sym.name_demangled().map(|n| frame_line(n, file.as_deref(), sym.lineno()))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n--\n");
        if ["hyper", "kube", "tower"].iter().any(|k| trace.contains(k)) {
            hits.push((t.id().to_string(), trace));
        }
    }
    let matched = hits.len();
    let (mut text, distinct) = render(&hits);
    let truncated = text.len() > max;
    if truncated {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Some(Dump { tasks, matched, distinct, ms, truncated, text })
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
    fn frames_keep_the_method_and_the_qualified_types() {
        let t = frame_line(
            "<hyper_util::client::legacy::client::Client<C,B> as tower_service::Service<http::request::Request<B>>>::call::{{closure}}::h0123456789abcdef",
            Some("/root/.cargo/registry/src/index.crates.io-abc/hyper-util-0.1.10/src/client/legacy/client.rs"),
            Some(233),
        );
        assert_eq!(t, "<Client<C,B> as Service<Request<B>>>::call::{{closure}} at hyper-util-0.1.10/src/client/legacy/client.rs:233");
        assert_eq!(
            frame_line("hyper::proto::h1::dispatch::Dispatcher<D,Bs,I,T>::poll_read_head", None, None),
            "hyper::proto::h1::dispatch::Dispatcher<D,Bs,I,T>::poll_read_head"
        );
        let long = frame_line(
            "tokio::runtime::task::Harness<hyper::client::conn::Connection<tokio_rustls::client::TlsStream<tokio::net::TcpStream>,F: Fn() -> Vec<u8>>>::poll",
            Some("/src/x.rs"),
            Some(7),
        );
        assert_eq!(long, "tokio::runtime::task::Harness<…>::poll at /src/x.rs:7");
    }

    #[test]
    fn identical_traces_print_once_callers_first() {
        let conn = "hyper_util::client::legacy::Client<A>::connect_to\n  tokio_rustls::Stream<B>::read".to_string();
        let tasks = vec![
            ("1".into(), conn.clone()),
            ("2".into(), conn.replace("<A>", "<Z, Y>")),
            ("3".into(), "kube_client::Client::send<W>\n  tower::buffer::Buffer::call".into()),
            ("4".into(), conn),
        ];
        let (text, distinct) = render(&tasks);
        assert_eq!(distinct, 2);
        assert!(text.starts_with("1x tasks [3]:\nkube_client::Client::send\n"), "{text}");
        assert!(text.contains("3x tasks [1,2,4]:\nhyper_util::client::legacy::Client::connect_to\n"), "{text}");
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
