//! Per-step timing for admin writes, so a hang is answered from the log alone.
//!
//! `POST /admin/requests/{id}/deny` went silent past the probe's 20 s twice (2026-09-12 23:56,
//! 09-13 14:21) with no `kube.timeout`, no `audit.slow` and no audit error: every await on the path
//! was under its own bound, and nothing said which one the time went to. So an admin handler runs
//! inside [`request`], and each external await on its path through [`step`]:
//!
//! - `admin.step` (debug) per step, `admin.step.slow` (warn) past [`SLOW`];
//! - `admin.step.stuck` (warn) every [`STUCK`] a step is STILL pending — a timer beside the
//!   future, never a timeout: behaviour is unchanged, only the silence is gone;
//! - `admin.request.done` (info) once, with the total and the slowest step.
//!
//! `step` outside a `request` scope (the same helpers serve `/v1`) is a plain await: no clock, no
//! line, so the user tier pays nothing. `tokio::time::Instant`, not std: the same clock in
//! production, and the one a paused test clock moves.

use std::cell::RefCell;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

pub(crate) const SLOW: Duration = Duration::from_secs(1);
pub(crate) const STUCK: Duration = Duration::from_secs(10);

struct Stats {
    id: String,
    slowest: &'static str,
    slowest_ms: u64,
}

tokio::task_local! {
    static REQ: RefCell<Stats>;
}

/// What a finished await is called in the log. A `Response` error is still an answer, not a hang.
pub(crate) trait Outcome {
    fn outcome(&self) -> &'static str;
}
impl<T, E> Outcome for Result<T, E> {
    fn outcome(&self) -> &'static str {
        if self.is_ok() { "ok" } else { "error" }
    }
}
impl Outcome for bool {
    fn outcome(&self) -> &'static str {
        if *self { "ok" } else { "refused" }
    }
}

/// Await `fut`, logging `admin.step.stuck` every `STUCK` it stays pending.
async fn watched<F: Future>(step: &str, id: &str, fut: F) -> F::Output {
    let start = Instant::now();
    let mut tick = tokio::time::interval_at(Instant::now() + STUCK, STUCK);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return out,
            _ = tick.tick() => {
                tracing::warn!(step, id, waited_ms = start.elapsed().as_millis() as u64, "admin.step.stuck");
            }
        }
    }
}

pub(crate) async fn step<F>(step: &'static str, fut: F) -> F::Output
where
    F: Future,
    F::Output: Outcome,
{
    let Ok(id) = REQ.try_with(|r| r.borrow().id.clone()) else {
        return fut.await;
    };
    let start = Instant::now();
    let out = watched(step, &id, fut).await;
    let elapsed = start.elapsed();
    let (elapsed_ms, outcome) = (elapsed.as_millis() as u64, out.outcome());
    if elapsed >= SLOW {
        tracing::warn!(step, id, elapsed_ms, outcome, "admin.step.slow");
    } else {
        tracing::debug!(step, id, elapsed_ms, outcome, "admin.step");
    }
    let _ = REQ.try_with(|r| {
        let mut r = r.borrow_mut();
        if elapsed_ms >= r.slowest_ms {
            (r.slowest, r.slowest_ms) = (step, elapsed_ms);
        }
    });
    out
}

/// Run one admin handler body as a timed request. `route` names it in every line.
pub(crate) async fn request<F>(route: &'static str, id: String, fut: F) -> F::Output
where
    F: Future,
    F::Output: Outcome,
{
    let start = Instant::now();
    let stats = RefCell::new(Stats { id: id.clone(), slowest: "none", slowest_ms: 0 });
    REQ.scope(stats, async move {
        let out = watched(route, &id, fut).await;
        let (slowest, slowest_ms) = REQ.with(|r| {
            let r = r.borrow();
            (r.slowest, r.slowest_ms)
        });
        let elapsed_ms = start.elapsed().as_millis() as u64;
        tracing::info!(route, id, elapsed_ms, outcome = out.outcome(), slowest, slowest_ms, "admin.request.done");
        out
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let b2 = buf.clone();
        let sub = kloudlite_core::log::subscriber(true, move || Sink(b2.clone()));
        (buf, tracing::subscriber::set_default(sub))
    }

    fn text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&buf.lock().unwrap()).to_string()
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_past_the_threshold_is_slow_and_named_in_done() {
        let (buf, _g) = capture();
        let out = request("request.deny", "req-1".into(), async {
            step("kube.patch.request", async {
                tokio::time::sleep(Duration::from_millis(1_500)).await;
                Ok::<(), ()>(())
            })
            .await
        })
        .await;
        assert!(out.is_ok());
        let out = text(&buf);
        assert!(out.contains("admin.step.slow"), "{out}");
        assert!(out.contains("\"step\":\"kube.patch.request\""), "{out}");
        assert!(out.contains("admin.request.done"), "{out}");
        assert!(out.contains("\"slowest\":\"kube.patch.request\""), "{out}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_step_says_stuck_while_it_waits() {
        let (buf, _g) = capture();
        let fut = request("workspace.stop", "ws-1".into(), async {
            step("kube.get.workspace", std::future::pending::<Result<(), ()>>()).await
        });
        assert!(tokio::time::timeout(Duration::from_secs(11), fut).await.is_err());
        let out = text(&buf);
        assert!(out.contains("admin.step.stuck"), "{out}");
        assert!(out.contains("\"step\":\"kube.get.workspace\""), "{out}");
        assert!(!out.contains("admin.request.done"), "{out}");
    }

    #[tokio::test]
    async fn outside_a_request_a_step_is_a_plain_await() {
        let (buf, _g) = capture();
        assert!(step("x", async { Ok::<(), ()>(()) }).await.is_ok());
        assert!(text(&buf).is_empty());
    }
}
