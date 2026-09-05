//! `PUT /admin/slo/runs/{id}` after every stage, so the console is a live step tracker.
//!
//! The whole run is re-sent each time rather than a delta: both tables are `ReplacingMergeTree`
//! keyed on the probe's own coordinates, so a lost PUT is repaired by the next one and there is
//! no partial state to reconcile on either side.

use kloudlite_git_workspaces::history::slo::{RunReport, RunState};

use crate::ctx::Ctx;

/// Three attempts, `retry_delay` apart, then the run exits 3. A report nobody stored is worse
/// than a failed run: it is a run the console never saw at all.
const ATTEMPTS: u32 = 3;

impl Ctx {
    pub async fn report(&mut self, stage: &str, finished: bool) -> anyhow::Result<()> {
        let report = RunReport {
            run_id: self.run_id.clone(),
            suite: self.suite.as_str().to_string(),
            region: self.cfg.region.clone(),
            started: self.started,
            finished: finished.then(chrono::Utc::now),
            state: match (finished, self.failed() > 0 || self.run_failed) {
                (false, _) => RunState::Running,
                (true, false) => RunState::Passed,
                (true, true) => RunState::Failed,
            },
            stage: stage.to_string(),
            steps: self.steps.clone(),
        };
        let url = format!("{}/admin/slo/runs/{}", self.cfg.admin_url.trim_end_matches('/'), self.run_id);
        let mut last = String::new();
        for attempt in 1..=ATTEMPTS {
            match self
                .http
                .put(&url)
                .header("authorization", self.bearer(&self.admin_jwt))
                .json(&report)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => return Ok(()),
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
            if attempt < ATTEMPTS {
                tokio::time::sleep(self.retry_delay).await;
            }
        }
        anyhow::bail!("{ATTEMPTS} attempts to PUT {url} failed: {last}")
    }
}

#[cfg(test)]
mod tests {
    use crate::testkit::{ctx, stub};
    use axum::http::StatusCode;

    #[tokio::test]
    async fn report_retries_three_times_then_errors() {
        let (url, hits) = stub(|| StatusCode::SERVICE_UNAVAILABLE).await;
        let mut c = ctx().await;
        c.cfg.admin_url = url;
        c.retry_delay = std::time::Duration::from_millis(1);
        let e = c.report("0 · Boot", false).await.unwrap_err().to_string();
        assert!(e.contains("3 attempts"), "{e}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
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
