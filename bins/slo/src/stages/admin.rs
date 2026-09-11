//! Stage 8 · Admin: the queue a person asks through, the audit row every admin write leaves, and
//! the two reads the console's own pages are built on.
//!
//! All four ids are HTTP against the admin process, so the whole stage is skipped by nothing —
//! there is no earlier stage it depends on. The one order that matters is inside it: `audit.row`
//! denies the request `req.queue` opened, because an audit row can only be checked for a write
//! that actually happened.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
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
    // A request a DEAD run left behind blocks this run's create — one pending per owner per kind,
    // and a status-less row is pending. Outside the step, so the sweep is not charged to the SLI.
    let swept = super::sweep_requests(c).await;
    if swept > 0 {
        tracing::info!(count = swept, "slo.requests.swept");
    }
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
    c.step("req.queue", QUEUE_CEILING, move |c| {
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
                // The CR itself goes in `sweep_requests`, by owner, whether or not this ran.
                c.state.request = Some(id.clone());
                poll_json(c, &queue, &admin_jwt, QUEUE_CEILING, |v| {
                    // `pending` rather than a literal comparison: a row the API server has not
                    // stamped a status onto yet IS pending to the api (`is_pending_generic`), and a
                    // probe that disagreed about that is what let a status-less row block a
                    // tenant's quota requests for a day.
                    rows(v).iter().any(|r| {
                        r.get("id").and_then(Value::as_str) == Some(id.as_str()) && super::pending(r)
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
    c.state.request.clone()
}

/// `audit.row`: the deny, and the append-only row it must leave behind.
///
/// The audit log is the legal record of every admin write (`crate::audit`), so this is the one
/// step where the interesting failure is a write that SUCCEEDED and left no trace.
async fn audit_row(c: &mut Ctx, id: &str) {
    let id = id.to_string();
    let ok = c.step("audit.row", AUDIT_CEILING, move |c| {
        let jwt = c.admin_jwt.clone();
        let deny = admin(c, &format!("/admin/requests/{id}/deny"));
        // `action` and `target` are exactly what `deny_request` records, so a row that comes back
        // is this deny's own and not some other admin's.
        let log = admin(c, &format!("/admin/audit?action=request.denied&target={id}"));
        // The dual write: every audit row is copied into `kloudlite.events` as `admin.<action>`,
        // where the console's own history charts read it. The object-store log stays the legal
        // record, so a ClickHouse that is not deployed is not a breach — that is `503 history
        // unavailable`, which this tolerates and nothing else does.
        let events = admin(c, "/admin/history/events?limit=200");
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
            .context("the deny left no audit row")?;
            dual_written(c, &events, &jwt, &id).await
        }
        .boxed()
    })
    .await;
    if !ok {
        // The step is already recorded; a 503 turns that sample into a skip, which is no sample.
        // Rewriting the row rather than checking first keeps the DENY — the write whose row this
        // id is about — on exactly one path.
        if let Some(step) = c.steps.iter_mut().rev().find(|s| s.slo_id == "audit.row") {
            if step.detail.contains(NO_HISTORY) {
                step.skipped = true;
                step.detail = NO_HISTORY.to_string();
            }
        }
    }
}

/// The same write, in `kloudlite.events` as `admin.request.denied`.
///
/// The invariant is per-write, not per-run: the object-store row and the ClickHouse row are two
/// halves of one guarantee, and an admin process that stopped consuming its own writes into the
/// history layer would leave the console's audit charts blank with the legal log intact — the one
/// failure a check on the object store alone cannot see.
///
/// A 503 is `KLOUDLITE_CLICKHOUSE_URL` being unset, which is a supported deployment and not an SLO
/// breach; every other answer is judged.
///
/// It is a SKIP, not a pass: an outage and an undeployed history layer answer identically here, so
/// a pass would have reported the per-write invariant as HELD exactly when half of it could not be
/// seen. `NO_HISTORY` is the marker `audit_row` turns into one — the deny itself has already
/// happened by then, and a skip is no sample rather than a green one.
async fn dual_written(c: &Ctx, url: &str, jwt: &str, target: &str) -> Result<()> {
    let (status, body) = super::raw(c, reqwest::Method::GET, url, jwt, None, &[]).await?;
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        tracing::info!(reason = "no clickhouse", "slo.admin.degraded");
        return Err(anyhow!("{NO_HISTORY}"));
    }
    if !status.is_success() {
        return Err(anyhow!("the history events read answered {status}"));
    }
    let seen: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let rows = seen.get("events").and_then(Value::as_array).or_else(|| seen.as_array()).cloned().unwrap_or_default();
    let there = rows.iter().any(|r| {
        let carried = r.to_string();
        carried.contains("admin.request.denied") && carried.contains(target)
    });
    there.then_some(()).ok_or_else(|| {
        anyhow!("the deny left an audit row but no `admin.request.denied` in kloudlite.events")
    })
}

/// `signals.fresh`: the Signals table is being fed by the alert evaluator rather than showing
/// every rule as `unknown`.
///
/// And the load-bearing half the SLI names second: a window the samples do not cover is `unknown`,
/// never `ok`. `signals` fills every (region, rule) the recorded set is missing with `unknown` and
/// the reason, so an `ok` carrying that filler reason is the evaluator's verdict and the fill's
/// detail on the same row — which is precisely the reading that retired the old on-request scrape.
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
            let rows = v.get("signals").and_then(Value::as_array).cloned().unwrap_or_default();
            let mine: Vec<&Value> = rows
                .iter()
                .filter(|r| r.get("region").and_then(Value::as_str) == Some(region.as_str()))
                .collect();
            evaluating(&mine, &region)
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
            // At least one bucket, not merely the key: `audit.row` writes an `admin.request.denied`
            // event every five minutes, so a seven-day window over audit events is empty only when
            // the migration never ran or the consumer stopped — the two failures an empty array
            // used to report as green.
            let series = v
                .get("series")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("the answer carried no series"))?;
            if series.is_empty() {
                return Err(anyhow!("seven days of audit events is empty, and this run writes one every five minutes"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// What `dual_written` answers when there is no ClickHouse to dual-write into, and the reason
/// `audit_row` files a skip instead of a sample.
const NO_HISTORY: &str = "the history read answers 503: no ClickHouse to dual-write into";

/// The reason nothing is filled in for this region, as the fill writes it. A row carrying it is a
/// rule the evaluator has never recorded a transition for.
const NO_SAMPLES: &str = "no collector reporting";

/// Both halves of the SLI over one region's rows.
///
/// A pure function so the judgement is testable without a fleet — and it is the judgement that
/// retired the old on-request scrape: a point-in-time read cannot compute a `for 5m` window, so it
/// left nine of ten rules `unknown`, and any reading that let an uncovered window report `ok`
/// would have called that healthy.
fn evaluating(rows: &[&Value], region: &str) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Err(anyhow!("the signals table lists no rule at all for {region}"));
    }
    let state = |r: &Value| r.get("state").and_then(Value::as_str).unwrap_or_default().to_string();
    // An uncovered window must be `unknown`. A row that carries the fill's own reason and any
    // other state is the evaluator having reported a verdict for samples it never had.
    if let Some(bad) = rows.iter().find(|r| {
        r.get("detail").and_then(Value::as_str).is_some_and(|d| d.contains(NO_SAMPLES))
            && state(r) != "unknown"
    }) {
        return Err(anyhow!(
            "`{}` reports `{}` for a window with no samples, which must be `unknown`",
            bad.get("alert").and_then(Value::as_str).unwrap_or("a rule"),
            state(bad)
        ));
    }
    // Every state is one of the three the evaluator writes: an unrecognised word renders as a
    // colourless cell nobody can act on.
    if let Some(bad) = rows.iter().find(|r| !STATES.contains(&state(r).as_str())) {
        return Err(anyhow!("a rule in {region} reports `{}`, which is not a state", state(bad)));
    }
    // And something is actually being evaluated.
    if rows.iter().all(|r| state(r) == "unknown") {
        return Err(anyhow!("every rule in {region} is unknown: nothing is evaluating"));
    }
    // Every RECORDED row names the moment it transitioned. A row with no `ts` is one the API filled
    // in for a rule that has never been recorded, and that row must carry the fill's own reason —
    // anything else is a state the console renders with no age against it at all.
    if let Some(bad) = rows.iter().find(|r| {
        r.get("ts").and_then(Value::as_str).is_none()
            && !r.get("detail").and_then(Value::as_str).is_some_and(|d| d.contains(NO_SAMPLES))
    }) {
        return Err(anyhow!(
            "`{}` reports `{}` with no timestamp: nothing says when it was recorded",
            bad.get("alert").and_then(Value::as_str).unwrap_or("a rule"),
            state(bad)
        ));
    }
    Ok(())
}

/// The four words `history::alerts` writes, and the fill's own.
/// The evaluator's own vocabulary (): a rule is , , or 
/// when its window had no samples. A firing rule is a valid state — the fleet may well have a
/// page open — and this check is about the WORDS, not the health;  failed on
/// 2026-09-11 the first time any rule ever fired, because this list had the old severities.
const STATES: [&str; 3] = ["ok", "firing", "unknown"];

/// The rows of a list route, whatever it wraps them in: `/admin/requests` answers a bare array.
fn rows(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule the old on-request scrape was retired for: a window the samples do not cover is
    /// `unknown`, never `ok`. A point-in-time read cannot compute a `for 5m`, so anything that let
    /// an uncovered window report a verdict would have called that healthy.
    #[test]
    fn an_uncovered_window_may_never_report_a_verdict() {
        let row = |alert: &str, state: &str, detail: &str| {
            serde_json::json!({ "alert": alert, "region": "r", "state": state, "detail": detail })
        };
        let row = |alert: &str, state: &str, detail: &str| {
            let mut v = row(alert, state, detail);
            // Every recorded row carries one; the fill's does not, which is the pair below.
            if !detail.contains("no collector reporting") {
                v["ts"] = serde_json::json!("2026-09-06 00:00:00");
            }
            v
        };
        let ok = [row("A", "ok", ""), row("B", "unknown", "no collector reporting for this region")];
        let rows: Vec<&Value> = ok.iter().collect();
        assert!(evaluating(&rows, "r").is_ok());
        // The failure: a rule with no samples reporting `ok`.
        let bad = [row("A", "ok", "no collector reporting for this region")];
        let rows: Vec<&Value> = bad.iter().collect();
        assert!(evaluating(&rows, "r").unwrap_err().to_string().contains("no samples"));
        // The failure the id already caught: nothing is evaluating at all.
        let dead = [row("A", "unknown", ""), row("B", "unknown", "")];
        let rows: Vec<&Value> = dead.iter().collect();
        assert!(evaluating(&rows, "r").unwrap_err().to_string().contains("nothing is evaluating"));
        // An empty table reads as "nothing is wrong" on the page, which is the one thing the fill
        // exists to prevent.
        assert!(evaluating(&[], "r").is_err());
        // A word nobody renders is a colourless cell.
        let odd = [row("A", "degraded", "")];
        let rows: Vec<&Value> = odd.iter().collect();
        assert!(evaluating(&rows, "r").is_err());
        // And a recorded row with no timestamp: the weakness the ms bound used to paper over.
        let undated = [serde_json::json!({ "alert": "A", "region": "r", "state": "ok", "detail": "" })];
        let rows: Vec<&Value> = undated.iter().collect();
        assert!(evaluating(&rows, "r").unwrap_err().to_string().contains("no timestamp"));
    }

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
