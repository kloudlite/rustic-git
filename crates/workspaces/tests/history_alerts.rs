//! The `for`-window decision and the catalogue's completeness. This is the half that is a rule
//! rather than plumbing, and the property that matters most is the one the previous, scrape-based
//! evaluator could not hold: a rule whose window is not fully covered says `unknown`, never `ok`.

use kloudlite_git_workspaces::history::alerts::{alert_row, evaluate_once, state_of, Tier, CATALOGUE};
use std::collections::HashMap;

/// One bucket per `step`, newest last, as `[ts, breached]` — the shape every catalogue query
/// returns so `state_of` is the single decision for all of them.
fn buckets(breaches: &[u8]) -> Vec<Vec<serde_json::Value>> {
    breaches
        .iter()
        .enumerate()
        .map(|(i, b)| vec![serde_json::json!(i), serde_json::json!(*b)])
        .collect()
}

#[test]
fn firing_needs_every_bucket_in_the_window_to_breach() {
    // 300 s of `for`, 30 s buckets: ten buckets, all breached.
    assert_eq!(state_of(&buckets(&[1; 10]), 300, 30).0, "firing");
}

/// One healthy bucket inside the window is what `for 5m` exists to tolerate — a blip must not page.
#[test]
fn one_healthy_bucket_inside_the_window_is_ok() {
    let mut b = [1u8; 10];
    b[4] = 0;
    assert_eq!(state_of(&buckets(&b), 300, 30).0, "ok");
}

/// The whole reason this evaluator replaced the scrape: a window the data does not cover cannot be
/// called healthy. A monitor that has only been up two minutes must not answer `ok` for a `for 5m`.
#[test]
fn a_window_that_is_not_covered_is_unknown_not_ok() {
    let (state, detail) = state_of(&buckets(&[1, 1, 1]), 300, 30);
    assert_eq!(state, "unknown");
    assert!(detail.contains("3 of 10"), "{detail}");
}

#[test]
fn no_data_at_all_is_unknown_with_a_reason() {
    let (state, detail) = state_of(&[], 300, 30);
    assert_eq!(state, "unknown");
    assert!(!detail.is_empty(), "an unknown must always say why");
}

/// A ClickHouse result is untrusted input: a row missing its `breached` column must read as not
/// breached, never panic the evaluator loop.
#[test]
fn a_malformed_row_does_not_panic() {
    let rows = vec![vec![serde_json::json!(0)], vec![serde_json::json!("x")]];
    assert_eq!(state_of(&rows, 60, 30).0, "ok");
}

/// Both evaluators read one catalogue (Global Constraints). If a rule is added to
/// `deploy/alerts.md` it must be added here, and the names must match exactly, or the console and
/// HyperDX disagree with no way to tell which is right.
#[test]
fn the_catalogue_matches_deploy_alerts_md() {
    let md = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/alerts.md"))
        .expect("deploy/alerts.md must be readable from the crate");
    for rule in CATALOGUE {
        assert!(md.contains(&format!("**{}**", rule.name)), "{} is not in deploy/alerts.md", rule.name);
        // Every rule must produce SQL for a region without panicking on the substitution.
        let sql = rule.sql_for("westeurope-k3s").expect("a region name is an identifier");
        assert!(sql.contains("westeurope-k3s"), "{} ignores its region", rule.name);
        assert!(sql.to_uppercase().starts_with("SELECT"), "{} is not a SELECT", rule.name);
    }
    // The reverse direction: every bolded alert name in the table has a rule here.
    for line in md.lines().filter(|l| l.starts_with("| **")) {
        let name = line.trim_start_matches("| **").split("**").next().unwrap();
        assert!(CATALOGUE.iter().any(|r| r.name == name), "{name} is in deploy/alerts.md but not in CATALOGUE");
    }
}

/// Every rule, by name: the metric it must read, the table, and the grouping the catalogue asks
/// for. A rename in the collector's tables or a lost `GROUP BY` shows up here rather than as a
/// silently permanent `unknown` (or, worse, a fleet-wide average hiding one sick node).
#[test]
fn every_rule_queries_its_own_metric_with_its_own_grouping() {
    // (rule, table, metric, fragments that must appear)
    let want: &[(&str, &str, &str, &[&str])] = &[
        ("NoLeader", "otel_metrics_gauge", "ownership_is_leader", &["INTERVAL 120 SECOND", "k8s.pod.name"]),
        ("LeaseRenewFailing", "otel_metrics_sum", "ownership_renew_failures_total", &["INTERVAL 180 SECOND", "k8s.pod.name"]),
        ("DbFenceDetected", "otel_metrics_sum", "db_fence_detected_total", &["INTERVAL 600 SECOND", "HAVING count() > 0"]),
        ("Http5xxRate", "otel_metrics_sum", "http_requests_total", &["INTERVAL 300 SECOND", "listener", "class", "'5xx'"]),
        ("MisdirectedWrites", "otel_metrics_sum", "http_requests_total", &["INTERVAL 600 SECOND", "'421'"]),
        ("ReconcileErrors", "otel_metrics_sum", "reconciles_total", &["INTERVAL 600 SECOND", "kind", "'error'"]),
        ("TunnelSaturation", "otel_metrics_gauge", "gateway_open_tunnels", &["INTERVAL 300 SECOND", "800"]),
        // kubeletstats publishes container restarts as a GAUGE, and the catalogue's window is the
        // whole hour rather than a bucket.
        ("WorkerHeartbeatStale", "otel_metrics_gauge", "k8s.container.restarts", &["INTERVAL 3600 SECOND", "HAVING count() > 0"]),
        ("PoolAlmostFull", "otel_metrics_gauge", "node_pool_bytes_used", &["k8s.node.name", "0.8"]),
        ("NodeDiskAlmostFull", "otel_metrics_gauge", "k8s.node.filesystem.usage", &["k8s.node.name", "0.85"]),
        // Azure Monitor's own metrics: one series per Azure resource, bucketed at Azure's publish
        // interval, and the "N of M" counted inside the query rather than as a `for` window.
        ("CosmosRuSaturation", "otel_metrics_gauge", "azure_normalizedruconsumption_maximum",
            &["azuremonitor.resource_id", "INTERVAL 300 SECOND", "INTERVAL 1800 SECOND", "v >= 80", "max(n) >= 3"]),
        ("CosmosUnavailable", "otel_metrics_gauge", "azure_serviceavailability_average",
            &["azuremonitor.resource_id", "INTERVAL 900 SECOND", "v < 99.9", "max(n) >= 1"]),
        ("CosmosLatencyHigh", "otel_metrics_gauge", "azure_serversidelatency_average",
            &["azuremonitor.resource_id", "INTERVAL 1800 SECOND", "v > 100", "max(n) >= 3"]),
        ("RedisMemoryHigh", "otel_metrics_gauge", "azure_usedmemorypercentage_maximum",
            &["azuremonitor.resource_id", "INTERVAL 60 SECOND", "INTERVAL 600 SECOND", "v >= 80", "max(n) >= 5"]),
        ("RedisLoadHigh", "otel_metrics_gauge", "azure_serverload_maximum",
            &["azuremonitor.resource_id", "INTERVAL 60 SECOND", "INTERVAL 600 SECOND", "v >= 80", "max(n) >= 5"]),
        ("RedisReplicationUnhealthy", "otel_metrics_gauge", "azure_georeplicationhealthy_minimum",
            &["azuremonitor.resource_id", "INTERVAL 300 SECOND", "v < 1", "max(n) >= 1"]),
        // The lookup floor is the load-bearing half: without it three requests that all miss read
        // as a 100% miss rate.
        ("RedisMissRateHigh", "otel_metrics_gauge", "azure_cachemisses_total",
            &["azure_cachehits_total", "azuremonitor.resource_id", "INTERVAL 600 SECOND", "misses + hits >= 100", "> 0.5"]),
    ];
    assert_eq!(want.len(), CATALOGUE.len(), "every rule must be covered here");
    for (name, table, metric, fragments) in want {
        let rule = CATALOGUE.iter().find(|r| r.name == *name).expect(name);
        let sql = rule.sql_for("eu").expect("a plain region name is an identifier");
        for f in [&format!("default.{table}")[..], metric].iter().chain(fragments.iter()) {
            assert!(sql.contains(f), "{name} is missing {f}: {sql}");
        }
        assert!(rule.for_secs >= 30, "{name} has a sub-bucket for window");
    }
    // Every counter delta is computed inside one series before it is summed: `max - min` across two
    // pods is the spread between two cumulative counters, not an increase.
    for rule in CATALOGUE.iter().filter(|r| (r.sql_for("eu").unwrap()).contains("otel_metrics_sum")) {
        let sql = rule.sql_for("eu").unwrap();
        assert!(sql.contains("k8s.pod.name"), "{} sums across series: {sql}", rule.name);
    }
}

/// Every Azure rule folds its whole window into ONE row, so `state_of` reads it as a single
/// bucket — the "3 of 6" is inside the SQL. `HAVING count() > 0` is what keeps a resource that is
/// not reporting at all an `unknown` rather than a healthy `ok`.
#[test]
fn the_azure_rules_are_single_bucket_and_absent_data_stays_unknown() {
    for name in [
        "CosmosRuSaturation", "CosmosUnavailable", "CosmosLatencyHigh",
        "RedisMemoryHigh", "RedisLoadHigh", "RedisReplicationUnhealthy", "RedisMissRateHigh",
    ] {
        let rule = CATALOGUE.iter().find(|r| r.name == name).expect(name);
        let sql = rule.sql_for("central").expect("central is an identifier");
        assert_eq!(rule.for_secs, 30, "{name} must be one bucket wide");
        assert!(sql.contains("HAVING count() > 0"), "{name} would call missing data ok: {sql}");
        assert!(sql.contains("GROUP BY resource") || sql.contains("GROUP BY ResourceAttributes['azuremonitor.resource_id']"),
            "{name} folds every Azure resource together: {sql}");
    }
}

/// A rule reads metrics only one tier emits. Evaluated in the other it can only ever write
/// `unknown`, which on the Signals page is indistinguishable from a collector that has died — so
/// the rule is not evaluated there at all.
#[test]
fn a_rule_is_evaluated_only_in_its_own_tier() {
    let by = |n: &str| CATALOGUE.iter().find(|r| r.name == n).expect(n);
    for name in ["NoLeader", "LeaseRenewFailing", "DbFenceDetected", "MisdirectedWrites", "WorkerHeartbeatStale"] {
        assert!(by(name).applies_to("central"), "{name} is central");
        assert!(!by(name).applies_to("westeurope-k3s"), "{name} must not run in a region");
    }
    // Azure Monitor exports the managed Cosmos and Redis under `region: central` only.
    for name in [
        "CosmosRuSaturation", "CosmosUnavailable", "CosmosLatencyHigh",
        "RedisMemoryHigh", "RedisLoadHigh", "RedisReplicationUnhealthy", "RedisMissRateHigh",
    ] {
        assert!(by(name).applies_to("central"), "{name} is central");
        assert!(!by(name).applies_to("westeurope-k3s"), "{name} must not run in a region");
    }
    for name in ["ReconcileErrors", "PoolAlmostFull", "NodeDiskAlmostFull", "TunnelSaturation"] {
        assert!(by(name).applies_to("westeurope-k3s"), "{name} is regional");
        assert!(!by(name).applies_to("central"), "{name} must not run centrally");
    }
    // HTTP is served by both tiers, so this one rule is genuinely both.
    assert!(by("Http5xxRate").applies_to("central"));
    assert!(by("Http5xxRate").applies_to("westeurope-k3s"));
    // Every rule belongs somewhere: an empty tier list would silently retire it.
    for rule in CATALOGUE {
        assert!(
            rule.tier.contains(&Tier::Central) || rule.tier.contains(&Tier::Region),
            "{} is evaluated nowhere",
            rule.name
        );
    }
}

/// The region is the only caller-shaped value in a rule's SQL, and it is checked rather than
/// escaped — anything that is not a Kubernetes-shaped name yields no query at all.
#[test]
fn a_region_that_is_not_an_identifier_yields_no_sql() {
    let rule = &CATALOGUE[0];
    assert!(rule.sql_for("westeurope-k3s").is_some());
    assert!(rule.sql_for("eu' OR '1'='1").is_none());
    assert!(rule.sql_for("").is_none());
    assert!(rule.sql_for("EU").is_none());
}

fn rows(breaches: &[u8]) -> Result<Vec<Vec<serde_json::Value>>, String> {
    Ok(buckets(breaches))
}

/// A beat writes only what CHANGED: `kloudlite.alerts` answers "when did this start", and a row per
/// evaluation would bury that under a hundred million rows saying nothing happened.
#[test]
fn a_second_identical_beat_writes_nothing() {
    let now = chrono::Utc::now();
    let mut last = HashMap::new();
    let results = vec![("NoLeader", rows(&[1, 1, 1, 1]))];
    let first = evaluate_once("eu", now, &results, &mut last);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["state"], serde_json::json!("firing"));
    assert!(evaluate_once("eu", now, &results, &mut last).is_empty());
    // …and a change is written again.
    let back = evaluate_once("eu", now, &[("NoLeader", rows(&[0, 0, 0, 0]))], &mut last);
    assert_eq!(back.len(), 1);
    assert_eq!(back[0]["state"], serde_json::json!("ok"));
}

/// A query that failed is its own state, recorded like any other — leaving yesterday's `ok`
/// standing would show the page a number nobody measured.
#[test]
fn a_query_error_transitions_to_unknown() {
    let now = chrono::Utc::now();
    let mut last = HashMap::new();
    evaluate_once("eu", now, &[("NoLeader", rows(&[0, 0, 0, 0]))], &mut last);
    let w = evaluate_once("eu", now, &[("NoLeader", Err("clickhouse 500".into()))], &mut last);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0]["state"], serde_json::json!("unknown"));
    assert!(w[0]["detail"].as_str().unwrap().contains("clickhouse 500"));
}

#[test]
fn an_alert_row_is_keyed_so_a_retried_write_collapses() {
    let ts = chrono::DateTime::parse_from_rfc3339("2026-09-04T10:00:00Z").unwrap().into();
    let a = alert_row(ts, "eu", "NoLeader", "firing", "sum = 0");
    let b = alert_row(ts, "eu", "NoLeader", "firing", "sum = 0");
    assert_eq!(a["id"], b["id"]);
    assert_eq!(a["ts"], serde_json::json!("2026-09-04 10:00:00"));
    assert_eq!(a["region"], serde_json::json!("eu"));
    assert_eq!(a["state"], serde_json::json!("firing"));
}

/// `current_signals` is the read half: the latest row per (region, rule) out of `kloudlite.alerts`,
/// mapped onto the shape the Signals table renders — including the catalogue's "Why", which is
/// carried in code rather than stored per row.
#[tokio::test]
async fn current_signals_maps_stored_rows_onto_the_response_shape() {
    use axum::{routing::post, Router};
    use kloudlite_git_workspaces::history::{alerts::current_signals, History};

    let app = Router::new().route(
        "/",
        post(|| async {
            serde_json::json!({"data": [
                ["eu", "NoLeader", "firing", "breached for all 4 buckets"],
                ["eu", "NotARule", "ok", ""]
            ]})
            .to_string()
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let rows = current_signals(&History::new(&format!("http://{addr}"), "u", "p"))
        .await
        .expect("a canned result parses");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].alert, "NoLeader");
    assert_eq!(rows[0].region, "eu");
    assert_eq!(rows[0].state, "firing");
    assert!(rows[0].why.starts_with("Zero: nobody holds the lease"));
    assert_eq!(rows[0].detail.as_deref(), Some("breached for all 4 buckets"));
    // A stored rule name the catalogue no longer has still renders — with an empty "why" rather
    // than a panic, since the row is a fact that was recorded and the page must not hide it.
    assert_eq!(rows[1].why, "");
    assert_eq!(rows[1].detail, None);
}
