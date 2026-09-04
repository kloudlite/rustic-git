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
    /// `region -> SQL`, private: every caller goes through `sql_for`, which is where the region's
    /// identifier check lives. A public field would be an unchecked way to build the same string.
    sql: fn(&str) -> String,
    pub for_secs: u64,
}

impl Rule {
    /// This rule's SQL for one region, or `None` if the region name is not an identifier we are
    /// willing to interpolate. The SQL itself is static text; the region is the only caller-shaped
    /// value that reaches it, so it is CHECKED rather than escaped — a Kubernetes object name is
    /// already `[a-z0-9.-]`, and anything else is a bug or an injection attempt.
    pub fn sql_for(&self, region: &str) -> Option<String> {
        let ok = !region.is_empty()
            && region.len() <= 253
            && region
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
        ok.then(|| (self.sql)(region))
    }
}

/// The identity of one cumulative counter SERIES: the pod that emits it and its label set. Every
/// delta below is computed inside one series and only then summed, because `max(Value) -
/// min(Value)` across two pods is the SPREAD between two independent cumulative counters, not an
/// increase — two pods at 10 and 1000 would read as a permanent +990 with nothing happening.
const SERIES: &str = "ResourceAttributes['k8s.pod.name'], Attributes";

/// A counter's per-bucket increase out of `otel_metrics_sum`: per-series `max - min` inside the
/// bucket, clamped at zero (a negative delta is a pod restart, not a decrease), then summed.
fn bucketed_sum(metric: &str, filter: &str, region: &str, window_secs: u64) -> String {
    format!(
        "SELECT b, sum(d) AS v FROM (\
            SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                   greatest(max(Value) - min(Value), 0) AS d \
            FROM default.otel_metrics_sum \
            WHERE MetricName = '{metric}' {filter} \
              AND ResourceAttributes['region'] = '{region}' \
              AND TimeUnix > now() - INTERVAL {window_secs} SECOND \
            GROUP BY b, {SERIES}) \
         GROUP BY b"
    )
}

/// The catalogue's two ratio rules, which differ only in metric, grouping and threshold: per-series
/// deltas first, folded into a ratio PER GROUP (a registry outage must not be hidden by healthy git
/// traffic), and the bucket breaches if any group is over. A bucket with no traffic is `ok` at
/// ratio 0 rather than dropped — dropping it would leave a quiet window uncovered, which
/// `state_of` would then have to call `unknown`.
fn ratio_rule(
    metric: &str,
    group: &[&str],
    bad_label: (&str, &str),
    threshold: f64,
    region: &str,
    window_secs: u64,
) -> String {
    let cols: Vec<String> = group
        .iter()
        .map(|g| format!("Attributes['{g}'] AS {g}"))
        .collect();
    let (cols, names) = (cols.join(", "), group.join(", "));
    let (bad_key, bad_value) = bad_label;
    format!(
        "SELECT b, toUInt8(max(r) > {threshold}) FROM (\
            SELECT b, {names}, sumIf(d, bad_label = '{bad_value}') AS bad, sum(d) AS total, \
                   if(total > 0, bad / total, 0) AS r \
            FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, {cols}, \
                       Attributes['{bad_key}'] AS bad_label, \
                       greatest(max(Value) - min(Value), 0) AS d \
                FROM default.otel_metrics_sum \
                WHERE MetricName = '{metric}' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL {window_secs} SECOND \
                GROUP BY b, {names}, bad_label, {SERIES}) \
            GROUP BY b, {names}) \
         GROUP BY b ORDER BY b"
    )
}

/// A rule whose window is the WHOLE catalogue window rather than a bucket (`increase(x[10m]) > 0`,
/// `increase(restarts[1h]) > 3`): one row, stamped at now, so `state_of` reads it as its single
/// bucket. `HAVING count() > 0` is what keeps "no series at all" an empty result — an aggregate
/// over nothing would otherwise answer 0 and read as a healthy `ok`.
fn whole_window(inner: &str, breached: &str) -> String {
    format!(
        "SELECT toStartOfInterval(now(), INTERVAL {STEP_SECS} SECOND) AS b, \
                toUInt8({breached}) \
         FROM ({inner}) HAVING count() > 0"
    )
}

/// The per-node worst ratio of two gauges — a fleet where one node is full and three are empty is
/// a node that is full, so the alert is `max` over nodes and never the fleet's busiest numerator
/// over its largest denominator.
fn worst_node_ratio(
    metrics: &str,
    numerator: &str,
    denominator: &str,
    threshold: f64,
    region: &str,
) -> String {
    format!(
        "SELECT b, toUInt8(max(r) > {threshold}) FROM (\
            SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                   ResourceAttributes['k8s.node.name'] AS node, \
                   {numerator}, \
                   {denominator}, \
                   if(den > 0, num / den, 0) AS r \
            FROM default.otel_metrics_gauge \
            WHERE MetricName IN ({metrics}) \
              AND ResourceAttributes['region'] = '{region}' \
              AND TimeUnix > now() - INTERVAL 300 SECOND \
            GROUP BY b, node) \
         GROUP BY b ORDER BY b"
    )
}

/// The catalogue, in `deploy/alerts.md`'s order and by its names.
pub const CATALOGUE: &[Rule] = &[
    Rule {
        name: "NoLeader",
        why: "Zero: nobody holds the lease, so no claim in the fleet succeeds; two: the epoch check failed and the fence is all that stands between two writers.",
        for_secs: 120,
        // `sum by (pod)` first: a pod reports this gauge several times inside a 30 s bucket, and a
        // flat `sum(Value)` would count one leader as many and never see `!= 1` for what it is.
        sql: |region| format!(
            "SELECT b, toUInt8(sum(pod_v) != 1) FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {STEP_SECS} SECOND) AS b, \
                       max(Value) AS pod_v \
                FROM default.otel_metrics_gauge \
                WHERE MetricName = 'ownership_is_leader' \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL 120 SECOND \
                GROUP BY b, ResourceAttributes['k8s.pod.name']) \
             GROUP BY b ORDER BY b"
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
        // The catalogue is `increase(db_fence_detected_total[10m]) > 0` with no `for`: any rise
        // anywhere in the ten minutes fires, and stays firing for the rest of them — a fence that
        // happened eight minutes ago is still the invariant violated.
        for_secs: STEP_SECS,
        sql: |region| whole_window(
            &format!(
                "SELECT greatest(max(Value) - min(Value), 0) AS d \
                 FROM default.otel_metrics_sum \
                 WHERE MetricName = 'db_fence_detected_total' \
                   AND ResourceAttributes['region'] = '{region}' \
                   AND TimeUnix > now() - INTERVAL 600 SECOND \
                 GROUP BY {SERIES}"
            ),
            "sum(d) > 0",
        ),
    },
    Rule {
        name: "Http5xxRate",
        why: "Per listener and route class so a registry outage is not hidden by healthy git traffic.",
        for_secs: 300,
        sql: |region| ratio_rule(
            "http_requests_total",
            &["listener", "class"],
            ("status", "5xx"),
            0.05,
            region,
            300,
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
        // The catalogue groups by `kind`: one controller in an error loop is the alert, and folding
        // it into the fleet's total reconciles would bury it.
        sql: |region| ratio_rule("reconciles_total", &["kind"], ("result", "error"), 0.2, region, 600),
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
        // The catalogue's window IS the hour: `increase(restarts[1h]) > 3` is one number, so the
        // rule has a single bucket rather than a `for` on top of it.
        for_secs: STEP_SECS,
        // `absent(up)` has no equivalent here, and inventing one from an empty result would fire on
        // every region that has no worker. The honest test is the catalogue's own second half: the
        // worker's restart count over the hour, per container — kubeletstats publishes
        // `k8s.container.restarts` as a GAUGE, so this reads the gauge table. An absent series
        // leaves the window uncovered, so `state_of` answers `unknown`, which is correct.
        sql: |region| whole_window(
            &format!(
                "SELECT greatest(max(Value) - min(Value), 0) AS d \
                 FROM default.otel_metrics_gauge \
                 WHERE MetricName = 'k8s.container.restarts' \
                   AND ResourceAttributes['k8s.container.name'] = 'worker' \
                   AND ResourceAttributes['region'] = '{region}' \
                   AND TimeUnix > now() - INTERVAL 3600 SECOND \
                 GROUP BY ResourceAttributes['k8s.pod.name']"
            ),
            "max(d) > 3",
        ),
    },
    Rule {
        name: "PoolAlmostFull",
        why: "btrfs past 80% starts failing allocations before df says full.",
        for_secs: 300,
        // The agent's own gauges (Task 7), which is what makes this rule evaluable at all — it was
        // permanently `unknown` while it depended on a node-exporter nobody deployed.
        sql: |region| worst_node_ratio(
            "'node_pool_bytes_used', 'node_pool_bytes_total'",
            "maxIf(Value, MetricName = 'node_pool_bytes_used') AS num",
            "maxIf(Value, MetricName = 'node_pool_bytes_total') AS den",
            0.8,
            region,
        ),
    },
    Rule {
        name: "NodeDiskAlmostFull",
        why: "The worker's merge caches and the slatedb object cache live on the root disk.",
        for_secs: 300,
        // `kubeletstats`' node filesystem metrics, so this needs no exporter of ours either.
        sql: |region| worst_node_ratio(
            "'k8s.node.filesystem.usage', 'k8s.node.filesystem.available'",
            "maxIf(Value, MetricName = 'k8s.node.filesystem.usage') AS num",
            "maxIf(Value, MetricName = 'k8s.node.filesystem.usage') + \
             maxIf(Value, MetricName = 'k8s.node.filesystem.available') AS den",
            0.85,
            region,
        ),
    },
];

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

/// A row for `kloudlite.alerts`. `ts` is when the transition was OBSERVED, and `id` is that instant
/// plus the transition's coordinates. The sort key is `(region, rule, ts, id)`, so FINAL can only
/// collapse rows that agree on `ts` too — which means a retry must carry the ORIGINAL row and never
/// a row re-stamped with the retrying beat's clock. `evaluate_forever` buffers rather than
/// recomputing for exactly that reason.
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
             FROM kloudlite.alerts FINAL GROUP BY region, rule",
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

/// What one rule's query came back with: its rows, or the error text, which is a state of its own.
pub type RuleResult<'a> = (&'a str, Result<Vec<Vec<serde_json::Value>>, String>);

/// One beat's worth of decisions for one region: `results` is `(rule name, rows or the query's own
/// error)`, and the return is the rows to insert — only the rules whose state CHANGED.
///
/// Factored out of the loop because this is the whole behaviour worth testing: a second identical
/// beat must write nothing, and a query that failed must transition to `unknown` rather than
/// leaving yesterday's `ok` standing on the page.
pub fn evaluate_once(
    region: &str,
    now: chrono::DateTime<chrono::Utc>,
    results: &[RuleResult<'_>],
    last: &mut HashMap<(String, String), String>,
) -> Vec<serde_json::Value> {
    let mut writes = Vec::new();
    for (name, result) in results {
        let Some(rule) = CATALOGUE.iter().find(|r| r.name == *name) else {
            continue;
        };
        let (st, detail) = match result {
            Ok(rows) => state_of(rows, rule.for_secs, STEP_SECS),
            // A failed query is not a healthy rule and not a stale one either: it is a state of its
            // own, recorded like any other so the page stops showing a number nobody measured.
            Err(e) => ("unknown", format!("query failed: {e}")),
        };
        let key = (region.to_string(), rule.name.to_string());
        if last.get(&key).map(String::as_str) != Some(st) {
            writes.push(alert_row(now, region, rule.name, st, &detail));
            last.insert(key, st.to_string());
        }
    }
    writes
}

/// The regions to evaluate: the `Region` CRs themselves, not `cluster_rows` — that walk lists every
/// Workspace, Environment and Node in the fleet plus one settings read per region, which is a lot
/// of API server for a list of names, every thirty seconds.
pub(crate) async fn region_names(state: &crate::api::ApiState) -> Option<Vec<String>> {
    use kube::api::{Api, ListParams, ResourceExt};
    let client = crate::api::kube(state).ok()?;
    let regions = Api::<crate::crd::Region>::all(client.clone())
        .list(&ListParams::default())
        .await
        .ok()?;
    Some(regions.iter().map(|r| r.name_any()).collect())
}

/// Evaluate every rule for every region on a 30 s beat, writing ONLY transitions.
///
/// Only transitions, because `kloudlite.alerts` answers "when did this start", and a row per
/// evaluation would turn a 400-day retention into a hundred million rows saying nothing changed.
pub async fn evaluate_forever(state: Arc<crate::api::ApiState>) {
    /// Roughly a day of transitions for a large fleet: enough that an outage of hours loses
    /// nothing, bounded so a week-long one cannot grow the process without limit.
    // ponytail: the oldest transitions are dropped first, because the newest state is what the
    // page renders. Upgrade path: spool to disk if a long outage must lose nothing.
    const MAX_PENDING: usize = 10_000;

    let mut last: HashMap<(String, String), String> = HashMap::new();
    let mut pending: Vec<serde_json::Value> = Vec::new();
    let mut iv = tokio::time::interval(EVERY);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        iv.tick().await;
        let Some(h) = state.history.as_deref() else {
            continue;
        };
        // A region list we could not read is not a reason to write "unknown" over a good state:
        // skip the beat and try again in thirty seconds.
        let Some(mut regions) = region_names(&state).await else {
            continue;
        };
        // The central tier (server, worker, gateway, api on AKS) is not a `Region` CR, but its
        // collectors stamp `central` and six of the rules read only its metrics — without this
        // line they sit at `unknown` forever.
        if !regions.iter().any(|r| r == super::watch::CENTRAL) {
            regions.push(super::watch::CENTRAL.to_string());
        }
        let now = chrono::Utc::now();
        let mut writes = std::mem::take(&mut pending);
        for region in &regions {
            let mut results = Vec::with_capacity(CATALOGUE.len());
            for rule in CATALOGUE {
                let Some(sql) = rule.sql_for(region) else {
                    // Never silent: a region whose name we refuse to interpolate is invisible on
                    // the Signals page, and only this line says why.
                    tracing::warn!(%region, "region name is not an identifier; alerts not evaluated");
                    break;
                };
                results.push((rule.name, h.query(&sql).await.map_err(|e| e.to_string())));
            }
            writes.extend(evaluate_once(region, now, &results, &mut last));
        }
        let wrote = h.insert("alerts", &writes).await;
        if let Err(e) = wrote {
            // The in-memory `last` already moved, so a dropped batch would simply be lost. The
            // ROWS are carried to the next beat rather than recomputed: recomputing would re-stamp
            // them from a later clock, and a row under a second `ts`/`id` is a second row FINAL
            // can never merge away.
            tracing::warn!(error = %e, n = writes.len(), "alert transitions not written; retrying next beat");
            pending = writes;
            if pending.len() > MAX_PENDING {
                pending.drain(..pending.len() - MAX_PENDING);
            }
        }
    }
}
