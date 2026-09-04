//! `deploy/alerts.md`, evaluated as SQL over the collector's metric tables, with the `for` windows
//! the catalogue actually specifies.
//!
//! Two evaluators, one catalogue (see the plan's Global Constraints): HyperDX alerts page a human,
//! this one fills the console's Signals table. A difference between them is a bug in one of them,
//! which is only findable because both use these exact rule names.
//!
//! Every rule's SQL returns the SAME shape — one row per `STEP_SECS` bucket over the window,
//! `[ts, breached]` with `breached` 1 or 0 — so `state_of` below is the single decision for all of
//! them and adding a rule is adding a query, never a new code path.
//!
//! Why this replaced the on-request scrape: the old module could see one instant (or two, five
//! seconds apart) and therefore had to answer `unknown` for every `for 5m` rule in the catalogue —
//! nine of ten rules were permanently unknown. With samples in ClickHouse the window is a `WHERE`
//! clause, and the rule that stays is the important one: a window the data does not cover is
//! `unknown`, never `ok`.

use super::{History, HistoryError};
use std::collections::HashMap;
use std::sync::Arc;

const TS_FMT: &str = "%Y-%m-%d %H:%M:%S";
/// The bucket width every rule is evaluated at. Thirty seconds is the collector's scrape interval
/// doubled, so a bucket always has at least one sample in it under normal operation.
const STEP_SECS: u64 = 30;
/// How often the loop runs. Faster than the shortest `for` window (2 m) so a transition is recorded
/// within a bucket of when it happened, and slow enough that ten queries are nothing.
const EVERY: std::time::Duration = std::time::Duration::from_secs(30);

pub struct Rule {
    pub name: &'static str,
    /// The catalogue's own "Why" column — carried so the console never has to restate it and the
    /// two can never drift.
    pub why: &'static str,
    /// `region -> SQL`. Returns `[bucket_ts, breached]` rows, newest last.
    pub sql: fn(&str) -> String,
    pub for_secs: u64,
}

/// A counter's per-bucket rate out of `otel_metrics_sum`, as a fragment every rate rule shares:
/// the exporter writes cumulative values, so a rate is `max - min` inside the bucket, and a
/// negative delta (a pod restart) is clamped to zero rather than poisoning the ratio.
fn bucketed_sum(metric: &str, filter: &str, region: &str, window_secs: u64) -> String {
    format!(
        "SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                greatest(max(Value) - min(Value), 0) AS v \
         FROM default.otel_metrics_sum \
         WHERE MetricName = '{metric}' {filter} \
           AND ResourceAttributes['region'] = '{region}' \
           AND TimeUnix > now() - INTERVAL {window_secs} SECOND \
         GROUP BY b"
    )
}

/// The catalogue, in `deploy/alerts.md`'s order and by its names.
pub const CATALOGUE: &[Rule] = &[
    Rule {
        name: "NoLeader",
        why: "Zero: nobody holds the lease, so no claim in the fleet succeeds; two: the epoch check failed and the fence is all that stands between two writers.",
        for_secs: 120,
        sql: |region| format!(
            "SELECT b, toUInt8(sum_v != 1) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       sum(Value) AS sum_v \
                FROM default.otel_metrics_gauge \
                WHERE MetricName = 'ownership_is_leader' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 120 SECOND \
                GROUP BY b) ORDER BY b"
        ),
    },
    Rule {
        name: "LeaseRenewFailing",
        why: "A node that cannot renew loses its leases at the TTL; another node claims, and its warm databases must close.",
        for_secs: 180,
        sql: |region| format!(
            "SELECT b, toUInt8(v > 0) FROM ({}) ORDER BY b",
            bucketed_sum("ownership_renew_failures_total", "", region, 180)
        ),
    },
    Rule {
        name: "DbFenceDetected",
        why: "The invariant violation: two nodes opened one SlateDB. Zero is the only acceptable value.",
        // No `for` in the catalogue — any rise at all fires, so one breached bucket is enough.
        for_secs: STEP_SECS,
        sql: |region| format!(
            "SELECT b, toUInt8(v > 0) FROM ({}) ORDER BY b",
            bucketed_sum("db_fence_detected_total", "", region, 600)
        ),
    },
    Rule {
        name: "Http5xxRate",
        why: "Per listener and route class so a registry outage is not hidden by healthy git traffic.",
        for_secs: 300,
        sql: |region| format!(
            "SELECT b, toUInt8(bad / total > 0.05) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       greatest(sumIf(Value, Attributes['status'] = '5xx') - \
                                minIf(Value, Attributes['status'] = '5xx'), 0) AS bad, \
                       greatest(sum(Value) - min(Value), 0) AS total \
                FROM default.otel_metrics_sum \
                WHERE MetricName = 'http_requests_total' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b HAVING total > 0) ORDER BY b"
        ),
    },
    Rule {
        name: "MisdirectedWrites",
        why: "421s during a roll are expected; sustained ones mean the pods disagree about who holds the leader lease.",
        for_secs: 600,
        sql: |region| format!(
            "SELECT b, toUInt8(v / {STEP_SECS} > 0.1) FROM ({}) ORDER BY b",
            bucketed_sum("http_requests_total", "AND Attributes['status'] = '421'", region, 600)
        ),
    },
    Rule {
        name: "ReconcileErrors",
        why: "A controller in an error loop keeps retrying with backoff; the ratio is what shows it.",
        for_secs: 300,
        sql: |region| format!(
            "SELECT b, toUInt8(bad / total > 0.2) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       greatest(sumIf(Value, Attributes['result'] = 'error') - \
                                minIf(Value, Attributes['result'] = 'error'), 0) AS bad, \
                       greatest(sum(Value) - min(Value), 0) AS total \
                FROM default.otel_metrics_sum \
                WHERE MetricName = 'reconciles_total' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b HAVING total > 0) ORDER BY b"
        ),
    },
    Rule {
        name: "TunnelSaturation",
        why: "MAX_TUNNELS is 1000 per gateway pod; refusals start with 503 past it.",
        for_secs: 300,
        sql: |region| format!(
            "SELECT b, toUInt8(m > 800) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       max(Value) AS m \
                FROM default.otel_metrics_gauge \
                WHERE MetricName = 'gateway_open_tunnels' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b) ORDER BY b"
        ),
    },
    Rule {
        name: "WorkerHeartbeatStale",
        why: "The liveness probe only restarts; this pages when it keeps happening.",
        for_secs: 300,
        // `absent(up)` has no equivalent here, and inventing one from an empty result would fire
        // on every region that has no worker. The honest test is the catalogue's own second half:
        // the worker's restart count rising. An absent series leaves the window uncovered, so
        // `state_of` answers `unknown` — which is correct, not a gap.
        sql: |region| format!(
            "SELECT b, toUInt8(v > 3) FROM ({}) ORDER BY b",
            bucketed_sum(
                "k8s.container.restarts",
                "AND ResourceAttributes['k8s.container.name'] = 'worker'",
                region,
                3600
            )
        ),
    },
    Rule {
        name: "PoolAlmostFull",
        why: "btrfs past 80% starts failing allocations before df says full.",
        for_secs: 300,
        // The agent's own gauges (Task 7), which is what makes this rule evaluable at all — it was
        // permanently `unknown` while it depended on a node-exporter nobody deployed.
        sql: |region| format!(
            "SELECT b, toUInt8(used / total > 0.8) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       maxIf(Value, MetricName = 'node_pool_bytes_used') AS used, \
                       maxIf(Value, MetricName = 'node_pool_bytes_total') AS total \
                FROM default.otel_metrics_gauge \
                WHERE MetricName IN ('node_pool_bytes_used', 'node_pool_bytes_total') \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b HAVING total > 0) ORDER BY b"
        ),
    },
    Rule {
        name: "NodeDiskAlmostFull",
        why: "The worker's merge caches and the slatedb object cache live on the root disk.",
        for_secs: 300,
        // `kubeletstats`' node filesystem metric, so this needs no exporter of ours either.
        sql: |region| format!(
            "SELECT b, toUInt8(used / (used + avail) > 0.85) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       maxIf(Value, MetricName = 'k8s.node.filesystem.usage') AS used, \
                       maxIf(Value, MetricName = 'k8s.node.filesystem.available') AS avail \
                FROM default.otel_metrics_gauge \
                WHERE MetricName IN ('k8s.node.filesystem.usage', 'k8s.node.filesystem.available') \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 300 SECOND \
                GROUP BY b HAVING used + avail > 0) ORDER BY b"
        ),
    },
];

/// A region name is the only caller-shaped text that reaches a rule's SQL, so it is checked as an
/// identifier before substitution rather than escaped: a Kubernetes object name is already
/// `[a-z0-9.-]`, and anything else is a bug or an injection attempt, never a region we should query.
fn is_region_ident(region: &str) -> bool {
    !region.is_empty()
        && region.len() <= 253
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
}

/// The one decision every rule goes through. `rows` is `[ts, breached]` newest last.
///
/// The order of the arms is the rule: a window that is not fully covered is `unknown` BEFORE it can
/// be `ok`, because "we did not look" and "we looked and it was fine" are different answers and
/// only one of them is safe to render green.
pub fn state_of(
    rows: &[Vec<serde_json::Value>],
    for_secs: u64,
    step_secs: u64,
) -> (&'static str, String) {
    let want = (for_secs / step_secs.max(1)).max(1) as usize;
    if rows.is_empty() {
        return (
            "unknown",
            "no samples in the window — is a collector reporting for this region?".into(),
        );
    }
    if rows.len() < want {
        return (
            "unknown",
            format!(
                "only {} of {want} buckets in the window have samples",
                rows.len()
            ),
        );
    }
    let recent = &rows[rows.len() - want..];
    let breached = recent
        .iter()
        // Query results are untrusted: a missing or non-numeric second column reads as "not
        // breached" rather than panicking the evaluator loop.
        .filter(|r| r.get(1).and_then(|v| v.as_u64()).unwrap_or(0) > 0)
        .count();
    if breached == want {
        (
            "firing",
            format!("breached for all {want} buckets of the {for_secs}s window"),
        )
    } else {
        (
            "ok",
            format!("breached {breached} of {want} buckets in the {for_secs}s window"),
        )
    }
}

/// A row for `rustic.alerts`. `id` is the coordinates of the transition, so a retried write of the
/// same transition collapses under the ReplacingMergeTree rather than doubling a count.
pub fn alert_row(
    ts: chrono::DateTime<chrono::Utc>,
    region: &str,
    rule: &str,
    state: &str,
    detail: &str,
) -> serde_json::Value {
    let ts = ts.format(TS_FMT).to_string();
    serde_json::json!({
        "ts": ts,
        "id": format!("{region}:{rule}:{state}:{ts}"),
        "region": region,
        "rule": rule,
        "state": state,
        "detail": detail,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SignalRow {
    pub alert: String,
    pub region: String,
    pub state: String,
    pub why: String,
    pub detail: Option<String>,
}

/// The latest state per `(region, rule)` — what the Signals table renders. A rule with no row at
/// all is `unknown` with the reason, added by the caller from `CATALOGUE`, so a region that has
/// never reported still shows ten rows rather than an empty table.
pub async fn current_signals(h: &History) -> Result<Vec<SignalRow>, HistoryError> {
    let rows = h
        .query(
            "SELECT region, rule, argMax(state, ts), argMax(detail, ts) \
             FROM rustic.alerts FINAL GROUP BY region, rule",
        )
        .await?;
    let why: HashMap<&str, &str> = CATALOGUE.iter().map(|r| (r.name, r.why)).collect();
    Ok(rows
        .iter()
        .map(|r| {
            let s = |i: usize| r.get(i).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let rule = s(1);
            SignalRow {
                why: why.get(rule.as_str()).copied().unwrap_or_default().to_string(),
                region: s(0),
                state: s(2),
                detail: Some(s(3)).filter(|d| !d.is_empty()),
                alert: rule,
            }
        })
        .collect())
}

/// Evaluate every rule for every region on a 30 s beat, writing ONLY transitions.
///
/// Only transitions, because `rustic.alerts` answers "when did this start", and a row per
/// evaluation would turn a 400-day retention into a hundred million rows saying nothing changed.
pub async fn evaluate_forever(state: Arc<crate::api::ApiState>) {
    let mut last: HashMap<(String, String), String> = HashMap::new();
    let mut iv = tokio::time::interval(EVERY);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        iv.tick().await;
        let Some(h) = state.history.as_deref() else {
            continue;
        };
        let regions = match crate::api::admin::clusters::cluster_rows(&state).await {
            Ok(rows) => rows.into_iter().map(|r| r.region).collect::<Vec<_>>(),
            // A region list we could not read is not a reason to write "unknown" over a good
            // state: skip the beat and try again in thirty seconds.
            Err(_) => continue,
        };
        let now = chrono::Utc::now();
        let mut writes = Vec::new();
        for region in regions.iter().filter(|r| is_region_ident(r)) {
            for rule in CATALOGUE {
                let rows = match h.query(&(rule.sql)(region)).await {
                    Ok(rows) => rows,
                    Err(e) => {
                        tracing::warn!(error = %e, rule = rule.name, %region, "alert query failed");
                        continue;
                    }
                };
                let (st, detail) = state_of(&rows, rule.for_secs, STEP_SECS);
                let key = (region.clone(), rule.name.to_string());
                if last.get(&key).map(String::as_str) != Some(st) {
                    writes.push(alert_row(now, region, rule.name, st, &detail));
                    last.insert(key, st.to_string());
                }
            }
        }
        if let Err(e) = h.insert("alerts", &writes).await {
            // The in-memory `last` already moved, so a failed write would suppress the retry.
            // Clearing it makes the next beat re-emit every transition it just computed.
            tracing::warn!(error = %e, n = writes.len(), "alert transitions not written; re-emitting next beat");
            last.clear();
        }
    }
}
