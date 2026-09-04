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

/// Which fleet a rule's metrics come from. `central` is the AKS tier (server, worker, gateway,
/// api); every `Region` CR is a k3s cluster with an agent on each node. A rule evaluated in the
/// wrong tier can only ever be `unknown` — the metric is not emitted there at all — and a
/// permanent `unknown` on the Signals page is indistinguishable from a broken collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Central,
    Region,
}

/// The tier one region name belongs to.
pub fn tier_of(region: &str) -> Tier {
    if region == super::watch::CENTRAL { Tier::Central } else { Tier::Region }
}

pub struct Rule {
    pub name: &'static str,
    /// The tiers this rule is evaluated in — a slice because `Http5xxRate` is genuinely both:
    /// the server tier and every region's api serve HTTP.
    pub tier: &'static [Tier],
    /// The catalogue's own "Why" column — carried so the console never has to restate it and the
    /// two can never drift.
    pub why: &'static str,
    /// `region -> SQL`, private: every caller goes through `sql_for`, which is where the region's
    /// identifier check lives. A public field would be an unchecked way to build the same string.
    sql: fn(&str) -> String,
    pub for_secs: u64,
}

impl Rule {
    /// Whether this rule is evaluated for a region at all.
    pub fn applies_to(&self, region: &str) -> bool {
        self.tier.contains(&tier_of(region))
    }

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

/// The Azure Monitor rules, which all have the same shape: one series per Azure resource in
/// `otel_metrics_gauge`, bucketed at the interval Azure actually publishes at (5 min for Cosmos,
/// 1 min for Redis — a 30 s bucket would be mostly empty), counted per resource, and the WORST
/// resource decides. `count` is the catalogue's "N of M": the whole window is one verdict, so
/// `for_secs` stays `STEP_SECS` and `state_of` reads the single row it returns.
#[allow(clippy::too_many_arguments)]
fn azure_rule(
    metric: &str,
    agg: &str,
    bucket_secs: u64,
    window_secs: u64,
    bad: &str,
    count: u64,
    resource_like: &str,
    region: &str,
) -> String {
    azure_rule_expr(
        metric,
        &format!("{agg}(Value)"),
        bucket_secs,
        window_secs,
        bad,
        count,
        resource_like,
        region,
    )
}

/// `azure_rule` with the bucket value as a full expression, for a metric Azure splits by
/// dimension (`MongoRequests` by `metadata_errorcode`) where one slice is the signal. A
/// `sumIf` over the unfiltered points, not a `WHERE`: the slice is absent when it is zero (no
/// throttled request means no 16500 point at all), and a filtered query would then return no
/// row and read as `unknown` rather than `ok`.
#[allow(clippy::too_many_arguments)]
fn azure_rule_expr(
    metric: &str,
    value_expr: &str,
    bucket_secs: u64,
    window_secs: u64,
    bad: &str,
    count: u64,
    // Two storage ACCOUNTS report the same metric names under one `region` (repo blobs and the
    // homes Files account), so the blob and homes rules differ ONLY by which resource id they
    // read — a parameter rather than a second copy of this query. Empty means every resource,
    // which is what the Cosmos and Redis rules want.
    resource_like: &str,
    region: &str,
) -> String {
    let resource_filter = if resource_like.is_empty() {
        String::new()
    } else {
        format!(" AND ResourceAttributes['azuremonitor.resource_id'] LIKE '{resource_like}'")
    };
    whole_window(
        &format!(
            "SELECT countIf({bad}) AS n FROM (\
                SELECT toStartOfInterval(TimeUnix, INTERVAL {bucket_secs} SECOND) AS b, \
                       ResourceAttributes['azuremonitor.resource_id'] AS resource, \
                       {value_expr} AS v \
                FROM default.otel_metrics_gauge \
                WHERE MetricName = '{metric}'{resource_filter} \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL {window_secs} SECOND \
                GROUP BY b, resource) \
             GROUP BY resource"
        ),
        &format!("max(n) >= {count}"),
    )
}

/// The blob account (every repo's SlateDB and every registry layer) and the Files account behind
/// `{pool}/homes`. Azure Monitor does not discover the blobServices/fileServices sub-resources, so
/// both report ACCOUNT-level metrics under the same names and the resource id is the only thing
/// that tells them apart.
const BLOB_ACCOUNT: &str = "%/storageAccounts/rusticgitkolomi";
const HOMES_ACCOUNT: &str = "%/storageAccounts/kloudlitegithomes";

/// A ratio of two DIFFERENT counter series (errors over requests), per group, over the whole
/// window — `ratio_rule`'s shape needs one metric split by a label, and here the numerator and the
/// denominator are separate metrics. The request floor is load-bearing for the same reason it is
/// in `RedisMissRateHigh`: one failed request out of three is not a 33% error rate worth waking
/// anyone for.
fn two_metric_ratio(
    bad_metric: &str,
    total_metric: &str,
    group: &str,
    threshold: f64,
    floor: u64,
    region: &str,
    window_secs: u64,
) -> String {
    whole_window(
        &format!(
            "SELECT sumIf(d, m = '{bad_metric}') AS bad, sumIf(d, m = '{total_metric}') AS total \
             FROM (\
                SELECT Attributes['{group}'] AS g, MetricName AS m, \
                       greatest(max(Value) - min(Value), 0) AS d \
                FROM default.otel_metrics_sum \
                WHERE MetricName IN ('{bad_metric}', '{total_metric}') \
                  AND ResourceAttributes['region'] = '{region}' \
                  AND TimeUnix > now() - INTERVAL {window_secs} SECOND \
                GROUP BY g, m, {SERIES}) \
             GROUP BY g"
        ),
        &format!("max(if(total >= {floor}, bad / total, 0)) > {threshold}"),
    )
}

/// The catalogue, in `deploy/alerts.md`'s order and by its names.
pub const CATALOGUE: &[Rule] = &[
    Rule {
        name: "NoLeader",
        tier: &[Tier::Central],
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
        tier: &[Tier::Central],
        why: "A node that cannot renew loses its leases at the TTL; another node claims, and its warm databases must close.",
        for_secs: 180,
        sql: |region| format!(
            "SELECT b, toUInt8(v > 0) FROM ({}) ORDER BY b",
            bucketed_sum("ownership_renew_failures_total", "", region, 180)
        ),
    },
    Rule {
        name: "DbFenceDetected",
        tier: &[Tier::Central],
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
        tier: &[Tier::Central, Tier::Region],
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
        tier: &[Tier::Central],
        why: "421s during a roll are expected; sustained ones mean the pods disagree about who holds the leader lease.",
        for_secs: 600,
        sql: |region| format!(
            "SELECT b, toUInt8(v / {STEP_SECS} > 0.1) FROM ({}) ORDER BY b",
            bucketed_sum("http_requests_total", "AND Attributes['status'] = '421'", region, 600)
        ),
    },
    Rule {
        name: "ReconcileErrors",
        tier: &[Tier::Region],
        why: "A controller in an error loop keeps retrying with backoff; the ratio is what shows it.",
        for_secs: 300,
        // The catalogue groups by `kind`: one controller in an error loop is the alert, and folding
        // it into the fleet's total reconciles would bury it.
        sql: |region| ratio_rule("reconciles_total", &["kind"], ("result", "error"), 0.2, region, 600),
    },
    Rule {
        name: "TunnelSaturation",
        tier: &[Tier::Region],
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
        tier: &[Tier::Central],
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
        tier: &[Tier::Region],
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
        tier: &[Tier::Region],
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
    Rule {
        name: "CosmosThrottled",
        tier: &[Tier::Central],
        why: "A serverless Mongo account publishes no RU consumption; Azure answers a request over the account's RU ceiling with error code 16500 (429), which the directory client retries and the sign-in path turns into an error. Ten in five minutes is a real ceiling, not a retry.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule_expr("azure_mongorequests_count", "sumIf(Value, Attributes['metadata_errorcode'] = '16500')", 300, 1800, "v >= 10", 1, "", region),
    },
    Rule {
        name: "CosmosUnavailable",
        tier: &[Tier::Central],
        why: "Every repo's SlateDB manifest and the ownership map live here; below the SLA the fleet is losing writes, not slowing down. Azure publishes availability at an hourly grain only, so this reads the last three hours.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule("azure_serviceavailability_average", "avg", 3600, 10800, "v < 99.9", 1, "", region),
    },
    Rule {
        name: "CosmosLatencyHigh",
        tier: &[Tier::Central],
        why: "Server-side latency is what Azure spent on the request, network excluded. A serverless Mongo account idles around 100 ms; 250 ms sustained means the account is being throttled short of a 429 or a partition is hot.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule("azure_serversidelatency_average", "avg", 300, 1800, "v > 250", 3, "", region),
    },
    Rule {
        name: "RedisMemoryHigh",
        tier: &[Tier::Central],
        why: "Past the maxmemory policy Redis starts evicting, and the events stream is what gets evicted — the fallbacks hold, but every consumer degrades to its beat.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule("azure_usedmemorypercentage_maximum", "max", 60, 600, "v >= 80", 5, "", region),
    },
    Rule {
        name: "RedisLoadHigh",
        tier: &[Tier::Central],
        why: "Server load is the fraction of the cycle spent busy; near 100 the instance stops accepting work rather than slowing down gracefully.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule("azure_serverload_maximum", "max", 60, 600, "v >= 80", 5, "", region),
    },
    Rule {
        name: "RedisReplicationUnhealthy",
        tier: &[Tier::Central],
        why: "Geo replication is the only copy of the stream outside one region; unhealthy is a silent state, and one minute of it is the whole signal.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule("azure_georeplicationhealthy_minimum", "min", 60, 300, "v < 1", 1, "", region),
    },
    Rule {
        name: "RedisMissRateHigh",
        tier: &[Tier::Central],
        // Two metrics, so this one cannot use `azure_rule` — but the shape is the same: one ratio
        // per resource over the whole window, worst resource decides. The 100-lookup floor is what
        // stops a nearly idle cache reading as 100% misses on three requests.
        why: "A cache that mostly misses is a cache nobody is served by; below the lookup floor the ratio is noise, so it is not evaluated.",
        for_secs: STEP_SECS,
        sql: |region| whole_window(
            &format!(
                "SELECT sumIf(Value, MetricName = 'azure_cachemisses_total') AS misses, \
                        sumIf(Value, MetricName = 'azure_cachehits_total') AS hits \
                 FROM default.otel_metrics_gauge \
                 WHERE MetricName IN ('azure_cachemisses_total', 'azure_cachehits_total') \
                   AND ResourceAttributes['region'] = '{region}' \
                   AND TimeUnix > now() - INTERVAL 600 SECOND \
                 GROUP BY ResourceAttributes['azuremonitor.resource_id']"
            ),
            "max(if(misses + hits >= 100, misses / (misses + hits), 0)) > 0.5",
        ),
    },
    Rule {
        name: "BlobUnavailable",
        tier: &[Tier::Central],
        why: "Every repo's SlateDB and every registry layer live in this account; below 99.9% availability the fleet is failing writes, not slowing down. Three of the last ten one-minute points, so a single blip does not page.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule("azure_availability_average", "avg", 60, 600, "v < 99.9", 3, BLOB_ACCOUNT, region),
    },
    Rule {
        name: "BlobLatencyHigh",
        tier: &[Tier::Central],
        why: "End-to-end latency includes the network and the client's own time, which is what a git request actually waits for. 500 ms sustained over five of the last ten minutes is well past the ~50 ms this account idles at.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule("azure_successe2elatency_average", "avg", 60, 600, "v > 500", 5, BLOB_ACCOUNT, region),
    },
    Rule {
        name: "BlobThrottled",
        tier: &[Tier::Central],
        why: "ServerBusy and ServerTimeout are the account's own IOPS ceiling answering 503; ten in one minute is a ceiling rather than the retry a client absorbs silently.",
        for_secs: STEP_SECS,
        // A `sumIf` over the unfiltered points, never a `WHERE` on the response type: a healthy
        // account emits no throttled point at all, and a filtered query would then return nothing
        // and read as `unknown` instead of `ok`.
        sql: |region| azure_rule_expr(
            "azure_transactions_count",
            "sumIf(Value, Attributes['metadata_responsetype'] IN ('ServerBusyError', 'ServerTimeoutError'))",
            60,
            600,
            "v >= 10",
            1,
            BLOB_ACCOUNT,
            region,
        ),
    },
    Rule {
        name: "HomesUnavailable",
        tier: &[Tier::Central],
        why: "The Files account behind every person's /home/kl. Below the SLA a workspace pod's home reads as an I/O error, not as slowness. Three of the last ten one-minute points.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule("azure_availability_average", "avg", 60, 600, "v < 99.9", 3, HOMES_ACCOUNT, region),
    },
    Rule {
        name: "HomesLatencyHigh",
        tier: &[Tier::Central],
        why: "Homes are on the NFS path of every shell prompt and every editor save, so the threshold is tighter than the blob account's: 200 ms for five of the last ten minutes is a home that feels broken.",
        for_secs: STEP_SECS,
        sql: |region| azure_rule("azure_successe2elatency_average", "avg", 60, 600, "v > 200", 5, HOMES_ACCOUNT, region),
    },
    Rule {
        name: "ProbeDown",
        tier: &[Tier::Central],
        // The httpcheck receiver writes one point per class per run, value 1 for the class that
        // matched, so "no 2xx or 4xx point" is the only honest reading of down — a 4xx is the
        // endpoint answering, which is up. A url that stopped emitting entirely leaves the window
        // uncovered and `HAVING count() > 0` keeps that `unknown`, never `ok`.
        why: "No 2xx or 4xx response from a probed url in two minutes (four probe runs) while it is still emitting — the endpoint is answering 5xx or not answering at all. The worst url decides; `httpcheck.error` by `error.message` says which and why.",
        for_secs: STEP_SECS,
        sql: |region| whole_window(
            &format!(
                "SELECT countIf(Attributes['http.status_class'] IN ('2xx', '4xx')) AS good \
                 FROM default.otel_metrics_gauge \
                 WHERE MetricName = 'httpcheck.status' \
                   AND Value = 1 \
                   AND ResourceAttributes['region'] = '{region}' \
                   AND TimeUnix > now() - INTERVAL 120 SECOND \
                 GROUP BY Attributes['http.url']"
            ),
            "countIf(good = 0) > 0",
        ),
    },
    Rule {
        name: "DependencyErrorRate",
        tier: &[Tier::Central],
        why: "Per dependency, so Cosmos failing is not averaged away by a healthy Redis. Over 5% of calls failing across five minutes, with at least 20 calls so a quiet dependency's single failure is not a 100% error rate.",
        for_secs: STEP_SECS,
        sql: |region| two_metric_ratio(
            "dependency_errors_total",
            "dependency_request_duration_seconds_count",
            "dep",
            0.05,
            20,
            region,
            300,
        ),
    },
    Rule {
        name: "WebErrorRate",
        tier: &[Tier::Central],
        why: "The web tier is the only thing a signed-in person actually talks to, and it reports NUMERIC statuses rather than the classes the Rust services emit. Over 5% 5xx per route across five minutes, with a 20-request floor.",
        for_secs: STEP_SECS,
        // Numeric status, hence `toUInt16OrZero` rather than the `class` label `Http5xxRate` uses;
        // an unparsable status reads as 0, which is not a 5xx — a bad label must not invent an
        // outage.
        sql: |region| whole_window(
            &format!(
                "SELECT sumIf(d, bad) AS bad, sum(d) AS total FROM (\
                    SELECT Attributes['route'] AS route, \
                           toUInt16OrZero(Attributes['status']) >= 500 AS bad, \
                           greatest(max(Value) - min(Value), 0) AS d \
                    FROM default.otel_metrics_sum \
                    WHERE MetricName = 'http_requests_total' \
                      AND ResourceAttributes['service.name'] = 'kloudlite-git-web' \
                      AND ResourceAttributes['region'] = '{region}' \
                      AND TimeUnix > now() - INTERVAL 300 SECOND \
                    GROUP BY route, bad, {SERIES}) \
                 GROUP BY route"
            ),
            "max(if(total >= 20, bad / total, 0)) > 0.05",
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
            // Only this region's tier: `TunnelSaturation` in `central` or `NoLeader` in a k3s
            // region reads a metric nothing there emits, so it could only ever write `unknown` —
            // a row that looks exactly like a collector that has stopped reporting.
            for rule in CATALOGUE.iter().filter(|r| r.applies_to(region)) {
                let Some(sql) = rule.sql_for(region) else {
                    // Never silent: a region whose name we refuse to interpolate is invisible on
                    // the Signals page, and only this line says why.
                    tracing::warn!(%region, reason = "region-not-an-identifier", "alerts.skipped");
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
            tracing::warn!(count = writes.len(), error = %e, "alerts.write.failed");
            pending = writes;
            if pending.len() > MAX_PENDING {
                pending.drain(..pending.len() - MAX_PENDING);
            }
        }
    }
}
