//! The `rustic` database, as numbered migrations the admin process applies at boot.
//!
//! `CREATE … IF NOT EXISTS` plus a recorded version, not a migration framework: a fresh ClickStack
//! becomes usable with no manual step, and an existing one skips what it already has. Never edit a
//! migration that has shipped — add the next number. The version is recorded only after the
//! statement returns, so a half-applied boot retries an idempotent statement rather than skipping
//! it.
//!
//! Migrations 6 and 7 read the COLLECTOR's tables. They are the one place our schema depends on
//! the exporter's, and they are written to fail loudly (a missing source table is a migration
//! error, logged at boot) rather than to silently produce an empty rollup — a chart that is flat
//! because a view never got built looks exactly like a chart that is flat because nothing happened.

use super::{History, HistoryError};

const DATABASE: &str = "CREATE DATABASE IF NOT EXISTS rustic";

/// The bookkeeping table. Applied before anything else and never numbered — it IS the numbering.
const BOOKKEEPING: &str = "CREATE TABLE IF NOT EXISTS rustic.schema_migrations \
    (version UInt32, applied_at DateTime DEFAULT now()) \
    ENGINE = ReplacingMergeTree ORDER BY version";

/// `(version, statement)`, ascending.
pub const MIGRATIONS: &[(u32, &str)] = &[
    // `events` is the record, so no TTL at all. ReplacingMergeTree on `id` is what makes
    // at-least-once safe: a replayed watch, a redelivered Redis entry and a retried insert all
    // collapse to one row, and every reader queries FINAL.
    (
        1,
        "CREATE TABLE IF NOT EXISTS rustic.events (\
            ts DateTime64(3), \
            id String, \
            kind LowCardinality(String), \
            actor String, \
            owner String, \
            target String, \
            region LowCardinality(String), \
            attrs String\
         ) ENGINE = ReplacingMergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (kind, ts, id)",
    ),
    // Recomputed from the CRDs every hour and never derived from an earlier row, so a plain
    // MergeTree is right: two beats in one hour are two honest observations, not a conflict.
    (
        2,
        "CREATE TABLE IF NOT EXISTS rustic.usage_hourly (\
            ts DateTime, \
            owner String, \
            is_team UInt8, \
            dimension LowCardinality(String), \
            used Float64, \
            `limit` Float64\
         ) ENGINE = MergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (owner, dimension, ts) \
           TTL ts + INTERVAL 730 DAY",
    ),
    (
        3,
        "CREATE TABLE IF NOT EXISTS rustic.fleet_hourly (\
            ts DateTime, \
            region LowCardinality(String), \
            nodes_total UInt32, \
            nodes_ready UInt32, \
            agents_ready UInt32, \
            live_workspaces UInt32, \
            live_environments UInt32, \
            snapshots UInt32, \
            disk_gb UInt64, \
            cpu UInt32, \
            memory_gb UInt32, \
            pool_used_bytes UInt64, \
            pool_total_bytes UInt64\
         ) ENGINE = MergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (region, ts) \
           TTL ts + INTERVAL 730 DAY",
    ),
    // Alerts are state TRANSITIONS, not one row per evaluation: the evaluator writes only when a
    // rule changes state, so the table stays small and "when did this start" is a plain lookup.
    // ReplacingMergeTree on `id` for the same at-least-once reason as `events`.
    (
        4,
        "CREATE TABLE IF NOT EXISTS rustic.alerts (\
            ts DateTime, \
            id String, \
            region LowCardinality(String), \
            rule LowCardinality(String), \
            state LowCardinality(String), \
            detail String\
         ) ENGINE = ReplacingMergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (region, rule, ts, id) \
           TTL ts + INTERVAL 400 DAY",
    ),
    // The 5-minute rollup the long sparklines (30 d / 90 d) read. The exporter's own `ttl` drops
    // raw metrics at 30 days, so without this a 90-day chart has nothing to draw past a month.
    // `region` and `node` are lifted out of the attribute maps at write time, because every read
    // filters on them and a Map lookup per row over 400 days of data is the difference between a
    // chart and a timeout.
    (
        5,
        "CREATE TABLE IF NOT EXISTS rustic.metrics_5m (\
            ts DateTime, \
            region LowCardinality(String), \
            node String, \
            metric LowCardinality(String), \
            attributes String, \
            avg_value AggregateFunction(avg, Float64), \
            max_value AggregateFunction(max, Float64), \
            last_value AggregateFunction(argMax, Float64, DateTime)\
         ) ENGINE = AggregatingMergeTree \
           PARTITION BY toYYYYMM(ts) \
           ORDER BY (region, metric, node, attributes, ts) \
           TTL ts + INTERVAL 400 DAY",
    ),
    // One view per source table. `otel_metrics_gauge` and `otel_metrics_sum` are the exporter's
    // (columns TimeUnix, MetricName, Value, Attributes, ResourceAttributes — verified against the
    // clickhouseexporter README); histograms are deliberately NOT rolled up, since averaging a
    // bucket count is meaningless and no console series asks for one.
    (
        6,
        "CREATE MATERIALIZED VIEW IF NOT EXISTS rustic.metrics_5m_gauge_mv TO rustic.metrics_5m AS \
         SELECT toStartOfFiveMinute(TimeUnix) AS ts, \
                ResourceAttributes['region'] AS region, \
                ResourceAttributes['k8s.node.name'] AS node, \
                MetricName AS metric, \
                toJSONString(Attributes) AS attributes, \
                avgState(Value) AS avg_value, \
                maxState(Value) AS max_value, \
                argMaxState(Value, TimeUnix) AS last_value \
         FROM default.otel_metrics_gauge \
         GROUP BY ts, region, node, metric, attributes",
    ),
    (
        7,
        "CREATE MATERIALIZED VIEW IF NOT EXISTS rustic.metrics_5m_sum_mv TO rustic.metrics_5m AS \
         SELECT toStartOfFiveMinute(TimeUnix) AS ts, \
                ResourceAttributes['region'] AS region, \
                ResourceAttributes['k8s.node.name'] AS node, \
                MetricName AS metric, \
                toJSONString(Attributes) AS attributes, \
                avgState(Value) AS avg_value, \
                maxState(Value) AS max_value, \
                argMaxState(Value, TimeUnix) AS last_value \
         FROM default.otel_metrics_sum \
         GROUP BY ts, region, node, metric, attributes",
    ),
];

/// Applies every migration this server has not recorded yet. Returns how many ran, so boot logs
/// "0" on the common path instead of a wall of statements.
pub async fn migrate(h: &History) -> Result<u32, HistoryError> {
    h.query(DATABASE).await?;
    h.query(BOOKKEEPING).await?;
    let done: Vec<u32> = h
        .query("SELECT version FROM rustic.schema_migrations FINAL")
        .await?
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_u64()).map(|v| v as u32))
        .collect();
    let mut applied = 0;
    for (version, sql) in MIGRATIONS {
        if done.contains(version) {
            continue;
        }
        h.query(sql).await?;
        // Recorded only after the statement returned: a crash in between re-runs an idempotent
        // `CREATE … IF NOT EXISTS`, which is the safe direction to be wrong in.
        h.insert(
            "schema_migrations",
            &[serde_json::json!({ "version": version })],
        )
        .await?;
        applied += 1;
    }
    Ok(applied)
}
