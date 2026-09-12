//! One SLO sample: run the closure, time it, record the outcome. Never propagate the failure.
//!
//! A step's own failure IS the measurement (the design's "Error handling"), so `step` returns a
//! bool rather than a `Result`: a caller that wants to abort the rest of a stage branches on it,
//! and one that does not simply ignores it — there is no `?` anywhere in a stage that could turn
//! one bad sample into a lost run.

use std::time::{Duration, Instant};

use chrono::Utc;
use futures::future::BoxFuture;
// The admin API's own ceiling, imported rather than repeated: a copy here would silently stop
// matching the day the validator's changed, and the whole report would start being refused.
use kloudlite_workspaces::history::slo::{SkipReason, StepReport, MAX_DETAIL};
use kloudlite_workspaces::slo::catalogue;

use crate::ctx::Ctx;

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
        // A step that failed WHILE a roll is in flight is a sample of the roll, not of the
        // service — the run's own guard only looked before it started. Asked only on a failure,
        // so a passing run costs nothing; the original detail stays, behind the reason. Every
        // suite: an hourly stop failed one second after its node's agent restarted (2026-09-11).
        // Bounded by ONE window per run (2026-09-12): a fleet wedged mid-roll used to turn every
        // failing sample of every run into a skip, so the console went quiet exactly when it
        // should have gone red.
        if !ok && crate::suite::rollout_in_flight(self).await && self.roll_window_open() {
            let why = format!("{}: {detail}", crate::suite::ROLLOUT_IN_FLIGHT);
            self.skip_because(id, &why, SkipReason::InFlight);
            self.save_state();
            return false;
        }
        // A step that WORKED but took longer than the catalogue promises is a bad sample, not a
        // good one (2026-09-12): the step's own ceiling is a generous timeout so a slow-but-alive
        // fleet is still measured, and until now the only thing that noticed the target was the
        // console's own maths — so a run whose every latency id was five times its target reported
        // `passed`. The ceiling stays the timeout; the TARGET is what judges the sample.
        // `ok` is what the JOURNEY asks — the workspace exists, so the next step may use it —
        // and `good` is the SAMPLE. Only the sample is judged against the target: failing the
        // journey on a slow-but-working fleet would cascade a skip through every dependent id and
        // cost the run its other measurements.
        let (good, detail) = match catalogue::find(id).and_then(|s| s.target.max_ms) {
            Some(max) if ok && ms > max => (false, format!("took {ms} ms, past its {max} ms target")),
            _ => (ok, detail),
        };
        tracing::info!(slo_id = id, ok = good, ms, detail = %detail, "slo.step.done");
        metrics::counter!("slo_steps_total", "ok" => if good { "true" } else { "false" }).increment(1);
        self.steps.push(StepReport {
            slo_id: id.to_string(),
            ts,
            ok: good,
            ms,
            skipped: false,
            detail: clip(detail),
            stage: self.stage.clone(),
            reason: SkipReason::None,
        });
        // After every step, not every stage: a child the parent kills at the wall-clock budget
        // hands over the names it recorded a second ago rather than the ones its last stage
        // boundary saw (`Ctx::save_state`).
        self.save_state();
        ok
    }

    /// A step whose precondition is gone. Skipped is NO SAMPLE — neither good nor bad — because
    /// the failure was already counted where it happened, and counting it twice would make one
    /// broken workspace look like eight broken SLOs.
    /// Whether a step already ran and passed — for a step that only makes sense after another
    /// (a promote after a build), where a missing prerequisite is a skip, never a failure.
    pub fn passed(&self, id: &str) -> bool {
        self.steps.iter().any(|s| s.slo_id == id && s.ok)
    }

    /// Turn the sample this run just filed for `id` into a SKIP.
    ///
    /// For a step whose assertion has two halves and whose second half could not be ATTEMPTED —
    /// a Kubernetes read with no kubeconfig. The alternative is a green sample for a property
    /// nobody checked, which is the false green this probe exists to prevent (2026-09-12).
    pub fn demote_to_skip(&mut self, id: &str, why: &str) {
        let Some(s) = self.steps.iter_mut().rev().find(|s| s.slo_id == id) else { return };
        tracing::info!(slo_id = id, reason = why, "slo.step.demoted");
        (s.ok, s.skipped, s.reason) = (false, true, SkipReason::Precondition);
        s.detail = clip(why.to_string());
    }

    /// The ordinary skip: a precondition an earlier step of this run did not produce, or the
    /// platform's own shape. Counted as a pass by `run_state`, because the failure — if there was
    /// one — was already counted where it happened.
    pub fn skip(&mut self, id: &'static str, why: &str) {
        self.skip_because(id, why, SkipReason::Precondition)
    }

    /// The same, with the machine-readable reason. `run_state` decides whether the whole run
    /// measured anything from THIS, never from the English in `detail`.
    pub fn skip_because(&mut self, id: &'static str, why: &str, reason: SkipReason) {
        tracing::info!(slo_id = id, reason = why, "slo.step.skipped");
        self.steps.push(StepReport {
            slo_id: id.to_string(),
            ts: Utc::now(),
            ok: false,
            ms: 0,
            skipped: true,
            detail: clip(why.to_string()),
            stage: self.stage.clone(),
            reason,
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
