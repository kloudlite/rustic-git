//! One SLO sample: run the closure, time it, record the outcome. Never propagate the failure.
//!
//! A step's own failure IS the measurement (the design's "Error handling"), so `step` returns a
//! bool rather than a `Result`: a caller that wants to abort the rest of a stage branches on it,
//! and one that does not simply ignores it — there is no `?` anywhere in a stage that could turn
//! one bad sample into a lost run.

use std::time::{Duration, Instant};

use chrono::Utc;
use futures::future::BoxFuture;
use kloudlite_git_workspaces::history::slo::StepReport;

use crate::ctx::Ctx;

/// The admin API refuses a longer one; truncating here means a stack-trace-sized error costs a
/// clipped message rather than the whole report.
const MAX_DETAIL: usize = 2000;

/// Every step gets one unless it names its own. Long enough that a slow-but-working fleet is
/// still measured rather than cut off, short enough that the fast suite fits its 540 s deadline.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

fn clip(mut s: String) -> String {
    if s.len() > MAX_DETAIL {
        // On a char boundary: `detail` is JSON, and a split codepoint is a report the admin
        // process cannot parse at all.
        let mut cut = MAX_DETAIL;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

impl Ctx {
    /// Run `f` under `timeout`, record the sample, return whether it passed.
    pub async fn step<F>(&mut self, id: &'static str, timeout: Duration, f: F) -> bool
    where
        F: for<'a> FnOnce(&'a mut Ctx) -> BoxFuture<'a, anyhow::Result<()>>,
    {
        let ts = Utc::now();
        let start = Instant::now();
        let outcome = tokio::time::timeout(timeout, f(self)).await;
        // Measured around the timeout, so a step that timed out reports the ceiling it hit rather
        // than a duration nobody recorded.
        let ms = start.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let (ok, detail) = match outcome {
            Ok(Ok(())) => (true, String::new()),
            // `{:#}` so an `anyhow` chain reads as "could not push: connection refused" rather
            // than only naming the outermost context.
            Ok(Err(e)) => (false, format!("{e:#}")),
            Err(_) => (false, format!("timed out after {} ms", timeout.as_millis())),
        };
        tracing::info!(slo_id = id, ok, ms, detail = %detail, "slo.step.done");
        metrics::counter!("slo_steps_total", "ok" => if ok { "true" } else { "false" }).increment(1);
        self.steps.push(StepReport {
            slo_id: id.to_string(),
            ts,
            ok,
            ms,
            skipped: false,
            detail: clip(detail),
            stage: self.stage.clone(),
        });
        ok
    }

    /// A step whose precondition is gone. Skipped is NO SAMPLE — neither good nor bad — because
    /// the failure was already counted where it happened, and counting it twice would make one
    /// broken workspace look like eight broken SLOs.
    pub fn skip(&mut self, id: &'static str, why: &str) {
        tracing::info!(slo_id = id, why, "slo.step.skipped");
        self.steps.push(StepReport {
            slo_id: id.to_string(),
            ts: Utc::now(),
            ok: false,
            ms: 0,
            skipped: true,
            detail: clip(why.to_string()),
            stage: self.stage.clone(),
        });
    }

    /// Whether any step so far is a real failure. Skips do not count, by the rule above.
    pub fn failed(&self) -> usize {
        self.steps.iter().filter(|s| !s.ok && !s.skipped).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::ctx;
    use futures::FutureExt;

    #[tokio::test]
    async fn a_step_records_ok_ms_and_detail() {
        let mut c = ctx().await;
        c.stage = "1 · Identity".into();
        let ok = c
            .step("id.signin", DEFAULT_TIMEOUT, |_| {
                async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(())
                }
                .boxed()
            })
            .await;
        assert!(ok);
        let s = &c.steps[0];
        assert!(s.ok && !s.skipped);
        assert!(s.ms >= 10, "ms {}", s.ms);
        assert_eq!(s.detail, "");
        assert_eq!(s.stage, "1 · Identity");
        assert_eq!(c.failed(), 0);
    }

    #[tokio::test]
    async fn a_timed_out_step_is_a_failure_not_a_panic() {
        let mut c = ctx().await;
        let ok = c
            .step("id.signin", Duration::from_millis(5), |_| {
                async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(())
                }
                .boxed()
            })
            .await;
        assert!(!ok);
        assert!(c.steps[0].detail.starts_with("timed out after"), "{}", c.steps[0].detail);
        assert_eq!(c.failed(), 1);
    }

    #[tokio::test]
    async fn a_failing_step_records_the_whole_error_chain() {
        let mut c = ctx().await;
        c.step("id.signin", DEFAULT_TIMEOUT, |_| {
            async { Err(anyhow::anyhow!("connection refused").context("could not sign in")) }.boxed()
        })
        .await;
        assert_eq!(c.steps[0].detail, "could not sign in: connection refused");
    }

    #[tokio::test]
    async fn skip_is_no_sample() {
        let mut c = ctx().await;
        c.skip("ws.exec.ok", "no workspace");
        let s = &c.steps[0];
        assert!(s.skipped && !s.ok);
        assert_eq!(s.detail, "no workspace");
        assert_eq!(c.failed(), 0, "a skip is not a failure");
    }

    #[tokio::test]
    async fn a_long_detail_is_clipped_on_a_char_boundary() {
        let mut c = ctx().await;
        c.skip("ws.exec.ok", &"é".repeat(4000));
        assert!(c.steps[0].detail.len() <= MAX_DETAIL);
        // Round-trips, which a split codepoint would not.
        assert!(c.steps[0].detail.chars().all(|ch| ch == 'é'));
    }
}
