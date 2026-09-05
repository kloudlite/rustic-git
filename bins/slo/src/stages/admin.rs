//! Stage 8 · Admin: the queue a person asks through, the audit row every admin write leaves, and
//! the two reads the console's own pages are built on.
//!
//! All four ids are HTTP against the admin process, so the whole stage is skipped by nothing —
//! there is no earlier stage it depends on. The one order that matters is inside it: `audit.row`
//! denies the request `req.queue` opened, because an audit row can only be checked for a write
//! that actually happened.

use std::time::Duration;

use anyhow::{anyhow, Context};
use futures::FutureExt;
use serde_json::Value;

use super::{admin, api, get, poll_json, post};
use crate::ctx::Ctx;

/// The catalogue bounds `req.queue` at 5 s; each ceiling here is looser than its target for the
/// usual reason — a slow answer must be a breach with a number rather than a step the probe cut
/// off — and the sum (60 s) is what stages 8, 9 and 10 together may cost the fast suite's 540 s
/// deadline, which is why none of them is generous.
const QUEUE_CEILING: Duration = Duration::from_secs(20);
const AUDIT_CEILING: Duration = Duration::from_secs(20);
const SIGNALS_CEILING: Duration = Duration::from_secs(10);
const HISTORY_CEILING: Duration = Duration::from_secs(10);

/// The `Request` kinds a probe may open safely: `other` is the only one whose APPROVAL would
/// change nothing (quota writes a `Quota`, access grants a role, region records a grant) — and the
/// probe denies its own request one step later anyway.
const KIND: &str = "other";

pub async fn run(c: &mut Ctx) {
    match queue(c).await {
        Some(id) => audit_row(c, &id).await,
        // The deny is the write whose row is looked for, so with no request there is nothing to
        // check — counted once, where it failed.
        None => c.skip("audit.row", "no request was queued"),
    }
    signals(c).await;
    history(c).await;
}

/// `req.queue`: a `Request` lands in the admin queue and is visible there as pending.
///
/// The reason MUST start with the run prefix: teardown finds a leftover request by that prefix and
/// denies it, and a pending request nobody denied blocks the NEXT run's own create (one pending
/// per owner per kind).
async fn queue(c: &mut Ctx) -> Option<String> {
    let reason = format!("{} slo probe", c.prefix());
    let ok = c
        .step("req.queue", QUEUE_CEILING, move |c| {
            let jwt = c.probe_jwt.clone();
            let admin_jwt = c.admin_jwt.clone();
            let url = api(c, "/v1/requests");
            let queue = admin(c, "/admin/requests");
            let body = serde_json::json!({
                "kind": KIND,
                "reason": reason,
                "other": { "title": "slo probe", "body": reason },
            });
            async move {
                let made = post(c, &url, &jwt, body).await.context("could not open a request")?;
                let id = made
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("the answer carried no request id"))?
                    .to_string();
                // Recorded before the wait, so teardown denies it even if the queue read fails.
                c.state.request = Some(id.clone());
                poll_json(c, &queue, &admin_jwt, QUEUE_CEILING, |v| {
                    rows(v).iter().any(|r| {
                        r.get("id").and_then(Value::as_str) == Some(id.as_str())
                            && r.get("state").and_then(Value::as_str) == Some("pending")
                    })
                })
                .await
                .context("the request never appeared as pending in the admin queue")
            }
            .boxed()
        })
        .await;
    // The id survives a failed WAIT: the request may well have been created, and denying it is
    // what keeps the next run's create from being refused.
    ok.then(|| c.state.request.clone()).flatten().or_else(|| c.state.request.clone())
}

/// `audit.row`: the deny, and the append-only row it must leave behind.
///
/// The audit log is the legal record of every admin write (`crate::audit`), so this is the one
/// step where the interesting failure is a write that SUCCEEDED and left no trace.
async fn audit_row(c: &mut Ctx, id: &str) {
    let id = id.to_string();
    c.step("audit.row", AUDIT_CEILING, move |c| {
        let jwt = c.admin_jwt.clone();
        let deny = admin(c, &format!("/admin/requests/{id}/deny"));
        // `action` and `target` are exactly what `deny_request` records, so a row that comes back
        // is this deny's own and not some other admin's.
        let log = admin(c, &format!("/admin/audit?action=request.denied&target={id}"));
        async move {
            post(c, &deny, &jwt, serde_json::json!({ "note": "slo probe" }))
                .await
                .context("could not deny the request")?;
            // The row is written after the response, best-effort by design — so this polls rather
            // than reading once.
            poll_json(c, &log, &jwt, AUDIT_CEILING, |v| {
                v.get("rows").and_then(Value::as_array).is_some_and(|r| !r.is_empty())
            })
            .await
            .context("the deny left no audit row")
        }
        .boxed()
    })
    .await;
}

/// `signals.fresh`: the Signals table is being fed by the alert evaluator rather than showing
/// every rule as `unknown`.
///
/// ponytail: freshness is read as "at least one rule in this region has a recorded state", because
/// the route carries no timestamp — `SignalRow` is (alert, region, state, why, detail). That is
/// the strongest reading available on the wire and it already catches the failure this SLO exists
/// for (the evaluator stopped, so every row falls back to `unknown`). Upgrade path: add the
/// transition's `ts` to `SignalRow` and compare its age here.
async fn signals(c: &mut Ctx) {
    c.step("signals.fresh", SIGNALS_CEILING, |c| {
        let jwt = c.admin_jwt.clone();
        let region = c.cfg.region.clone();
        let url = admin(c, "/admin/monitoring/signals");
        async move {
            let v = get(c, &url, &jwt).await.context("could not read the signals table")?;
            if v.get("source").and_then(Value::as_str) != Some("history") {
                return Err(anyhow!("no rule state has been recorded at all"));
            }
            let known = v
                .get("signals")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter(|r| r.get("region").and_then(Value::as_str) == Some(region.as_str()))
                        .any(|r| r.get("state").and_then(Value::as_str) != Some("unknown"))
                })
                .unwrap_or(false);
            known
                .then_some(())
                .ok_or_else(|| anyhow!("every rule in {region} is unknown: nothing is evaluating"))
        }
        .boxed()
    })
    .await;
}

/// `history.api`: one of the console's chart series answers. Any catalogue series would do; this
/// one reads `kloudlite.events`, which the admin process writes itself, so a green step means the
/// whole path — collector, ClickHouse, the query — is up.
async fn history(c: &mut Ctx) {
    c.step("history.api", HISTORY_CEILING, |c| {
        let jwt = c.admin_jwt.clone();
        let url = admin(c, "/admin/history/audit_events?range=7d&step=1d");
        async move {
            let v = get(c, &url, &jwt).await.context("the history API would not answer")?;
            // An empty series is fine — a quiet week is not a broken pipeline. The `series` key
            // being there at all is the answer this SLI is about.
            v.get("series")
                .and_then(Value::as_array)
                .map(|_| ())
                .ok_or_else(|| anyhow!("the answer carried no series"))
        }
        .boxed()
    })
    .await;
}

/// The rows of a list route, whatever it wraps them in: `/admin/requests` answers a bare array.
fn rows(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing reachable: every id is still produced exactly once, as a failure with a reason,
    /// never a missing sample.
    #[tokio::test]
    async fn admin_produces_every_id_even_with_nothing_reachable() {
        let mut c = crate::testkit::ctx().await;
        run(&mut c).await;
        let ids: Vec<&str> = c.steps.iter().map(|s| s.slo_id.as_str()).collect();
        assert_eq!(ids, ["req.queue", "audit.row", "signals.fresh", "history.api"]);
    }
}
