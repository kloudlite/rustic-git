//! One JSON line to `KLOUDLITE_GIT_SLO_WEBHOOK` when a probe run fails and when `SloBurn` starts
//! firing.
//!
//! Best effort by construction: the webhook is a NUDGE, exactly like the Redis events stream — the
//! record is `kloudlite.slo_runs` and `kloudlite.alerts`, both already written before this is
//! called, and HyperDX pages a human independently. So a failed post is logged and counted, never
//! retried and never turned into a 5xx for a report that already landed.

/// Long enough for a chat webhook to answer, short enough that the admin process is never held on
/// one: this is called from a request handler's success path and from the alert beat.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub async fn post(url: &str, body: &serde_json::Value) {
    let r = reqwest::Client::new()
        .post(url)
        .timeout(TIMEOUT)
        .json(body)
        .send()
        .await
        .and_then(|r| r.error_for_status());
    match r {
        Ok(_) => metrics::counter!("slo_notify_total", "result" => "ok").increment(1),
        Err(e) => {
            // Never the url: a webhook url is a secret (it is what authenticates the caller), and
            // a log line is the one place it would leak from.
            tracing::warn!(error = %e, "slo.notify.failed");
            metrics::counter!("slo_notify_total", "result" => "error").increment(1);
        }
    }
}

/// The body both callers send. `kind` is `slo.run.failed` or `slo.burning`; `console` is the page
/// that shows the rest, which is the only thing a person reading the line actually needs.
pub fn body(
    kind: &str,
    run_id: &str,
    suite: &str,
    failed_step: &str,
    detail: &str,
) -> serde_json::Value {
    serde_json::json!({
        "kind": kind,
        "run_id": run_id,
        "suite": suite,
        "failed_step": failed_step,
        "detail": detail,
        "console": console_url(),
    })
}

/// The console link. `KLOUDLITE_GIT_WEB_URL` is read here rather than carried in `ApiState` for the
/// same reason `monitoring`'s HyperDX link is: it is a constant of the deployment, and an unset one
/// leaves a relative path rather than a dead absolute link.
fn console_url() -> String {
    let base = std::env::var("KLOUDLITE_GIT_WEB_URL").unwrap_or_default();
    format!("{}/superadmin/slo", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_body_carries_the_console_link_and_the_failed_step() {
        let b = super::body("slo.run.failed", "fast-7", "fast", "git.push.ok", "boom");
        assert_eq!(b["kind"], "slo.run.failed");
        assert_eq!(b["failed_step"], "git.push.ok");
        assert!(b["console"].as_str().unwrap().ends_with("/superadmin/slo"));
    }
}
