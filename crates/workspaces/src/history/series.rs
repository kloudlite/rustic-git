//! The named series the console charts, one SQL statement each.
//!
//! A fixed catalogue, not a query language: the caller names a series and this module decides what
//! that means. Everything variable — the range, the step, the region, the owner — goes through an
//! allow-list or an identifier check before it is anywhere near a statement, because ClickHouse's
//! HTTP interface has no bound parameters on this path and a caller-supplied string in a query is
//! the only injection surface this crate has.

/// The allow-listed ranges, in days.
pub fn parse_range(range: &str) -> Option<u32> {
    match range {
        "7d" => Some(7),
        "30d" => Some(30),
        "90d" => Some(90),
        _ => None,
    }
}

/// The allow-listed steps, as the ClickHouse bucketing function each one means.
pub fn parse_step(step: &str) -> Option<&'static str> {
    match step {
        "1h" => Some("toStartOfHour"),
        "1d" => Some("toStartOfDay"),
        _ => None,
    }
}

/// An owner slug, a region id and a dimension word are all `[a-z0-9-_]`. Anything else is refused
/// rather than escaped: escaping is a thing to get subtly wrong, and no legitimate value here has
/// ever needed it.
pub(crate) fn ident(s: &str) -> Option<&str> {
    (!s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .then_some(s)
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct SeriesQuery {
    #[serde(default = "default_range")]
    pub range: String,
    #[serde(default = "default_step")]
    pub step: String,
    pub region: Option<String>,
    pub owner: Option<String>,
    pub dimension: Option<String>,
}

fn default_range() -> String {
    "7d".into()
}
fn default_step() -> String {
    "1h".into()
}

#[derive(Debug, serde::Serialize)]
pub struct Summary {
    pub last: f64,
    pub delta: f64,
    pub min: f64,
    pub max: f64,
}

/// Zeros, not NaN, on an empty series: NaN serializes as `null` and renders as a broken chart on a
/// cluster whose only fault is being new.
pub fn summarize(points: &[(String, f64)]) -> Summary {
    let values: Vec<f64> = points.iter().map(|(_, v)| *v).collect();
    match (values.first(), values.last()) {
        (Some(first), Some(last)) => Summary {
            last: *last,
            delta: last - first,
            min: values.iter().copied().fold(f64::INFINITY, f64::min),
            max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        },
        _ => Summary {
            last: 0.0,
            delta: 0.0,
            min: 0.0,
            max: 0.0,
        },
    }
}

/// `None` means "no such series" — the route's 404 — and also covers a series whose required
/// parameter is missing or malformed, since both are "this request names nothing we can answer".
pub fn sql_for(series: &str, q: &SeriesQuery) -> Option<String> {
    let days = parse_range(&q.range)?;
    let bucket = parse_step(&q.step)?;
    // Validated once, here, so no statement below has to remember to.
    let region = match q.region.as_deref() {
        Some(r) => Some(ident(r)?),
        None => None,
    };
    let region_filter = region
        .map(|r| format!("AND region = '{r}'"))
        .unwrap_or_default();

    // `metrics_5m` keeps one aggregate state per (node, attribute set), so a fleet number has to
    // fold each node's own last value first — a bare `argMaxMerge` over the whole bucket would
    // answer with whichever node reported last, not with the fleet.
    let per_node = |metric: &str| {
        format!(
            "SELECT {bucket}(ts) AS b, node, argMaxMerge(last_value) AS v \
             FROM kloudlite.metrics_5m \
             WHERE metric = '{metric}' AND ts > now() - INTERVAL {days} DAY {region_filter} \
             GROUP BY b, node"
        )
    };
    // ponytail: the collector's `*.utilization` metrics are ratios where kubeletstats emits them
    // as such; a collector configured to emit cores or bytes instead would draw off the top of a
    // percentage axis, so the value is clamped rather than trusted. Upgrade path: divide by the
    // node's allocatable once `k8s.node.allocatable_*` is in the pipeline.
    let ratio_series = |metric: &str| {
        format!(
            "SELECT b, least(greatest(avg(v), 0), 1) AS v FROM ({}) GROUP BY b ORDER BY b",
            per_node(metric)
        )
    };
    let event_count = |kinds: &str| {
        format!(
            "SELECT {bucket}(ts) AS b, count() AS v FROM kloudlite.events FINAL \
             WHERE kind IN ({kinds}) AND ts > now() - INTERVAL {days} DAY {region_filter} \
             GROUP BY b ORDER BY b"
        )
    };
    let fleet_max = |column: &str| {
        format!(
            "SELECT {bucket}(ts) AS b, max({column}) AS v FROM kloudlite.fleet_hourly \
             WHERE ts > now() - INTERVAL {days} DAY {region_filter} GROUP BY b ORDER BY b"
        )
    };

    Some(match series {
        // A request is pending from the hour it opened until the hour it was decided; the running
        // difference of the two event kinds is what makes that a series rather than a
        // point-in-time number.
        "pending_requests" => format!(
            "SELECT b, sum(v) OVER (ORDER BY b) AS running FROM (\
                SELECT {bucket}(ts) AS b, \
                       toInt64(countIf(kind = 'request.opened')) - \
                       toInt64(countIf(kind IN ('request.approved', 'request.denied'))) AS v \
                FROM kloudlite.events FINAL \
                WHERE kind LIKE 'request.%' AND ts > now() - INTERVAL {days} DAY \
                      {region_filter} \
                GROUP BY b) ORDER BY b"
        ),
        "decided_requests" => event_count("'request.approved', 'request.denied'"),
        // CENTRAL-ONLY, so no `region_filter`: `events::audit_event` stamps every audit row
        // `region: "central"`, and a region-scoped call would draw a flat zero. The `admin.`
        // namespace is the contract rather than a list of actions, which goes stale the day
        // somebody adds one.
        "audit_events" => format!(
            "SELECT {bucket}(ts) AS b, count() AS v FROM kloudlite.events FINAL \
             WHERE kind LIKE 'admin.%' AND ts > now() - INTERVAL {days} DAY \
             GROUP BY b ORDER BY b"
        ),
        "live_workspaces" => fleet_max("live_workspaces"),
        "live_environments" => fleet_max("live_environments"),
        // A RATIO, like every other `*_used` series: the console draws all three on one percentage
        // axis, so bytes here would render as an off-scale spike.
        "pool_used" => format!(
            "SELECT {bucket}(ts) AS b, \
                    max(pool_used_bytes) / nullIf(max(pool_total_bytes), 0) AS v \
             FROM kloudlite.fleet_hourly \
             WHERE ts > now() - INTERVAL {days} DAY {region_filter} GROUP BY b ORDER BY b"
        ),
        // `kloudlite.alerts` holds TRANSITIONS only, so a rule that began firing before the range and
        // never changed since has no row in any bucket — a per-bucket argMax over the range would
        // draw the longest-running alert in the fleet as not firing. State is carried forward
        // instead: buckets are generated from the range, every (region, rule) is paired with every
        // bucket, and each pair takes the newest transition at or before that bucket, read over the
        // WHOLE table rather than the range. A rule with no transition at all yields `''`.
        "firing_signals" => format!(
            "SELECT b, countIf(s = 'firing') AS v FROM (\
                SELECT b, region, rule, argMaxIf(state, ts, ts <= b) AS s \
                FROM (SELECT {bucket}(now() - INTERVAL n HOUR) AS b \
                      FROM numbers({days} * 24)) AS buckets \
                CROSS JOIN (SELECT ts, region, rule, state FROM kloudlite.alerts FINAL \
                            WHERE 1 = 1 {region_filter}) AS a \
                GROUP BY b, region, rule) GROUP BY b ORDER BY b"
        ),
        // An owner counts once per bucket if ANY dimension is past 80% — the number the Overview
        // strip shows is "owners who need attention", not "owner-dimension pairs".
        "owners_over_80" => format!(
            "SELECT b, uniqExact(owner) AS v FROM (\
                SELECT {bucket}(ts) AS b, owner FROM kloudlite.usage_hourly \
                WHERE ts > now() - INTERVAL {days} DAY AND `limit` > 0 AND used / `limit` > 0.8) \
             GROUP BY b ORDER BY b"
        ),
        "time_to_decide_p50" => format!(
            "SELECT b, quantile(0.5)(secs) AS v FROM (\
                SELECT {bucket}(decided) AS b, \
                       dateDiff('second', opened, decided) AS secs FROM (\
                    SELECT target, \
                           minIf(ts, kind = 'request.opened') AS opened, \
                           maxIf(ts, kind IN ('request.approved', 'request.denied')) AS decided \
                    FROM kloudlite.events FINAL \
                    WHERE kind LIKE 'request.%' AND ts > now() - INTERVAL {days} DAY \
                          {region_filter} \
                    GROUP BY target \
                    HAVING decided > opened)) \
             GROUP BY b ORDER BY b"
        ),
        "cpu_used" => ratio_series("k8s.node.cpu.utilization"),
        "memory_used" => ratio_series("k8s.node.memory.utilization"),
        // `k8s.container.restarts` is CUMULATIVE per container, so the value in a bucket is a
        // running total, not the bucket's restarts. Per (node, container) the rise across the
        // bucket is max - min; summed, that is "restarts that happened in this bucket" — which is
        // what a bar on the chart claims to be. Grouped by `attributes` too, or two containers on
        // one node collapse into whichever reported last.
        "restarts" => format!(
            "SELECT b, sum(rise) AS v FROM (\
                SELECT b, node, attributes, greatest(max(v) - min(v), 0) AS rise FROM (\
                    SELECT {bucket}(ts) AS b, ts, node, attributes, \
                           maxMerge(max_value) AS v \
                    FROM kloudlite.metrics_5m \
                    WHERE metric = 'k8s.container.restarts' \
                      AND ts > now() - INTERVAL {days} DAY {region_filter} \
                    GROUP BY b, ts, node, attributes) \
                GROUP BY b, node, attributes) GROUP BY b ORDER BY b"
        ),
        "usage" => {
            let owner = ident(q.owner.as_deref()?)?;
            let dimension = ident(q.dimension.as_deref()?)?;
            format!(
                "SELECT {bucket}(ts) AS b, max(used) AS v FROM kloudlite.usage_hourly \
                 WHERE owner = '{owner}' AND dimension = '{dimension}' \
                   AND ts > now() - INTERVAL {days} DAY GROUP BY b ORDER BY b"
            )
        }
        _ => return None,
    })
}
