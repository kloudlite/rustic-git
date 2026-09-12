//! `PUT /admin/slo/runs/{id}` after every stage, so the console is a live step tracker.
//!
//! The whole run is re-sent each time rather than a delta: both tables are `ReplacingMergeTree`
//! keyed on the probe's own coordinates, so a lost PUT is repaired by the next one and there is
//! no partial state to reconcile on either side.

use kloudlite_workspaces::history::slo::{RunReport, RunState, SkipReason};

use crate::ctx::Ctx;

/// Six attempts over a minute — 2, 4, 8, 16, 30 s apart — then the run exits 3. A minute rather
/// than the six seconds three fixed attempts bought: the admin process is a Deployment that rolls,
/// and a roll takes longer than six seconds. A run lost to a deploy is a run the console cannot
/// tell apart from a CronJob that never fired.
const BACKOFF: [u64; 5] = [2, 4, 8, 16, 30];

/// Above this the probe's own `ts` on every step is a lie the console cannot see: the run's samples
/// would land in a window they did not happen in. Reported as a warning rather than a failure —
/// the fleet is fine, the pod's clock is not.
const MAX_SKEW_MS: i64 = 30_000;

/// How long to wait after `attempt`, or `None` when there is nothing left to wait for.
fn backoff(attempt: u32, unit: std::time::Duration) -> Option<std::time::Duration> {
    BACKOFF.get(attempt as usize - 1).map(|n| unit * *n as u32)
}

/// The run's own verdict, in precedence order: a failure beats everything, a run that yielded
/// wholesale is `yielded`, and a run that skipped ANY id at all is `skipped` — never `passed`.
///
/// The last rule is 2026-09-12's: hourly-1789188203 met the roll guard after stage 2, skipped
/// every step from there on and finished `passed` with `failed: 0`, so the operator's own
/// `run-job.sh` printed "hourly passed" for a run that proved nothing. A skipped step is NO
/// SAMPLE, and a verdict built out of no samples is not evidence the fleet is well — the skips
/// are still not counted as failures, they simply stop the run being called a pass.
pub fn run_state(finished: bool, failed: bool, steps: &[kloudlite_workspaces::history::slo::StepReport]) -> RunState {
    match (finished, failed) {
        (false, _) => RunState::Running,
        (true, true) => RunState::Failed,
        // The REASON, never the English in `detail` (2026-09-12): rewording a skip message must
        // not silently turn a run that measured nothing into a run that passed.
        (true, false)
            if !steps.is_empty()
                && steps.iter().all(|s| s.skipped && s.reason == SkipReason::InFlight) =>
        {
            RunState::Yielded
        }
        (true, false) if steps.iter().any(|s| s.skipped) => RunState::Skipped,
        (true, false) => RunState::Passed,
    }
}

impl Ctx {
    pub async fn report(&mut self, stage: &str, finished: bool) -> anyhow::Result<()> {
        let report = RunReport {
            run_id: self.run_id.clone(),
            suite: self.suite.as_str().to_string(),
            region: self.cfg.region.clone(),
            started: self.started,
            finished: finished.then(chrono::Utc::now),
            state: run_state(finished, self.failed() > 0 || self.run_failed, &self.steps),
            stage: stage.to_string(),
            steps: self.steps.clone(),
        };
        let url = format!("{}/admin/slo/runs/{}", self.cfg.admin_url.trim_end_matches('/'), self.run_id);
        let mut last = String::new();
        let attempts = BACKOFF.len() as u32 + 1;
        for attempt in 1..=attempts {
            match self
                .http
                .put(&url)
                .header("authorization", self.bearer(&self.admin_jwt()))
                .json(&report)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    self.note_skew(r.headers());
                    return Ok(());
                }
                // A 400 is a report this probe will never be able to file — a bad run id, an slo
                // id not in the catalogue — so retrying it two more times only delays the exit.
                Ok(r) if r.status() == reqwest::StatusCode::BAD_REQUEST => {
                    let s = r.status();
                    let body = r.text().await.unwrap_or_default();
                    anyhow::bail!("{s} from {url}: {body}");
                }
                Ok(r) => {
                    let s = r.status();
                    last = format!("{s}: {}", r.text().await.unwrap_or_default());
                }
                Err(e) => last = e.to_string(),
            }
            tracing::warn!(attempt, error = %last, "slo.report.retried");
            // `retry_delay` is the UNIT the schedule is measured in, one second in a deployment;
            // a test shrinks it rather than reimplementing the schedule.
            if let Some(d) = backoff(attempt, self.retry_delay) {
                tokio::time::sleep(d).await;
            }
        }
        anyhow::bail!("{attempts} attempts to PUT {url} failed: {last}")
    }

    /// The admin process's own `Date`, against this pod's clock.
    ///
    /// Recorded once per run, from the first report that landed: every step's `ts` is the probe's,
    /// and the console's windows are the admin's — so a pod whose clock has drifted files samples
    /// into windows they did not happen in, and nothing downstream can tell. This is the only
    /// place the two clocks are ever in the same room.
    fn note_skew(&mut self, headers: &reqwest::header::HeaderMap) {
        if self.clock_skew_ms.is_some() {
            return;
        }
        let Some(theirs) = headers
            .get(reqwest::header::DATE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| chrono::DateTime::parse_from_rfc2822(v).ok())
        else {
            return;
        };
        let skew = (chrono::Utc::now() - theirs.with_timezone(&chrono::Utc)).num_milliseconds();
        self.clock_skew_ms = Some(skew);
        tracing::info!(run_id = %self.run_id, clock_skew_ms = skew, "slo.clock.skew");
        if skew.abs() > MAX_SKEW_MS {
            tracing::warn!(run_id = %self.run_id, reason = "clock skew", clock_skew_ms = skew, "slo.run.warned");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run_state;
    use crate::testkit::{ctx, stub};
    use axum::http::StatusCode;
    use kloudlite_workspaces::history::slo::{RunState, SkipReason, StepReport};

    fn step(ok: bool, skipped: bool, detail: &str) -> StepReport {
        let reason = match (skipped, detail.contains("in flight")) {
            (true, true) => SkipReason::InFlight,
            (true, false) => SkipReason::Precondition,
            _ => SkipReason::None,
        };
        StepReport { slo_id: "x".into(), ts: chrono::Utc::now(), ok, ms: 0, skipped, detail: detail.into(), stage: "1".into(), reason }
    }

    /// The reason is what decides it — a yielded run whose English was reworded is still a
    /// yielded run, and a `Precondition` skip that happens to mention a flight is not one.
    #[test]
    fn the_reason_decides_a_yield_not_the_wording() {
        let mut reworded = step(true, true, "a longer run of this suite already has the platform");
        reworded.reason = SkipReason::InFlight;
        assert_eq!(run_state(true, false, &[reworded]), RunState::Yielded);
        let mut shaped = step(true, true, "the workspace this id needs is in flight");
        shaped.reason = SkipReason::Precondition;
        // Not `Yielded` — the wording says "in flight" and the reason does not — and not a pass
        // either, because an id was skipped.
        assert_eq!(run_state(true, false, &[shaped]), RunState::Skipped);
    }

    #[test]
    fn a_run_that_only_yielded_is_not_a_pass() {
        let yielded = vec![step(true, true, "an hourly run is in flight"), step(true, true, "a monthly drill is in flight")];
        assert_eq!(run_state(true, false, &yielded), RunState::Yielded);
        assert_eq!(run_state(false, false, &yielded), RunState::Running, "still running while it reports");
        let mixed = vec![step(true, true, "an hourly run is in flight"), step(true, false, "")];
        assert_eq!(run_state(true, false, &mixed), RunState::Skipped, "a partly skipped run is not a pass");
        let shape = vec![step(true, true, "no kubeconfig")];
        assert_eq!(run_state(true, false, &shape), RunState::Skipped, "a run that skipped an id is not a pass");
        assert_eq!(run_state(true, true, &yielded), RunState::Failed);
        assert_eq!(run_state(true, false, &[]), RunState::Passed, "no steps at all is the old behaviour");
    }

    /// The fleet's own case, 2026-09-12: a roll after stage 2 left one measured step and a pile
    /// of skips, and the run read `passed`. `failed` still wins over `skipped`.
    #[test]
    fn one_skipped_step_is_not_a_pass_and_a_failure_still_wins() {
        let one = vec![step(true, false, ""), step(true, true, "a rollout is in flight: could not push")];
        assert_eq!(run_state(true, false, &one), RunState::Skipped);
        assert_eq!(run_state(true, true, &one), RunState::Failed, "a failure outranks a skip");
        let clean = vec![step(true, false, ""), step(true, false, "")];
        assert_eq!(run_state(true, false, &clean), RunState::Passed);
    }

    #[tokio::test]
    async fn report_retries_six_times_then_errors() {
        let (url, hits) = stub(|| StatusCode::SERVICE_UNAVAILABLE).await;
        let mut c = ctx().await;
        c.cfg.admin_url = url;
        c.retry_delay = std::time::Duration::from_millis(1);
        let e = c.report("0 · Boot", false).await.unwrap_err().to_string();
        assert!(e.contains("6 attempts"), "{e}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 6);
    }

    /// The schedule, not merely the count: an admin roll takes longer than the six seconds three
    /// fixed attempts bought, and the whole point of the backoff is that it outlasts one. Slept on
    /// a paused clock, so the test proves a minute of patience without spending one — and against
    /// the real `backoff`, so shortening the schedule fails here rather than in a deploy.
    #[tokio::test(start_paused = true)]
    async fn the_backoff_waits_a_full_minute_before_giving_up() {
        let unit = std::time::Duration::from_secs(1);
        let at = tokio::time::Instant::now();
        let mut attempt = 1;
        while let Some(d) = super::backoff(attempt, unit) {
            tokio::time::sleep(d).await;
            attempt += 1;
        }
        assert_eq!(at.elapsed().as_secs(), 60);
        // Increasing, so a fleet that is slow to come back is asked less often, not more.
        assert!(super::BACKOFF.windows(2).all(|w| w[0] < w[1]));
        // And the unit scales the WHOLE schedule, which is what lets a test shrink it.
        assert_eq!(super::backoff(1, unit / 1000).unwrap().as_millis(), 2);
    }

    /// The two clocks in one room. A pod whose clock has drifted stamps every step with a `ts` the
    /// console's windows disagree with, and nothing downstream can see it — so the skew is read off
    /// the one response that comes from the process that owns those windows.
    #[tokio::test]
    async fn the_first_stored_report_records_the_clock_skew() {
        let (url, _) = stub(|| StatusCode::NO_CONTENT).await;
        let mut c = ctx().await;
        c.cfg.admin_url = url;
        c.report("0 · Boot", false).await.unwrap();
        // axum stamps a `Date` from the same machine, so the skew is real and near zero — what is
        // asserted is that one was computed at all, not that two clocks agree to the millisecond.
        let skew = c.clock_skew_ms.expect("a skew was recorded");
        assert!(skew.abs() < 5_000, "{skew}");
    }

    #[tokio::test]
    async fn a_stored_report_is_one_request() {
        let (url, hits) = stub(|| StatusCode::NO_CONTENT).await;
        let mut c = ctx().await;
        c.cfg.admin_url = url;
        c.report("0 · Boot", false).await.unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A malformed report is a bug in the probe, not a flaky admin process — one attempt, and the
    /// message names what the server said so the exit-3 log is actionable.
    #[tokio::test]
    async fn a_bad_request_is_not_retried() {
        let (url, hits) = stub(|| StatusCode::BAD_REQUEST).await;
        let mut c = ctx().await;
        c.cfg.admin_url = url;
        c.retry_delay = std::time::Duration::from_millis(1);
        assert!(c.report("0 · Boot", false).await.is_err());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
