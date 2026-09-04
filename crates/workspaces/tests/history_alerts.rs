//! The `for`-window decision and the catalogue's completeness. This is the half that is a rule
//! rather than plumbing, and the property that matters most is the one the previous, scrape-based
//! evaluator could not hold: a rule whose window is not fully covered says `unknown`, never `ok`.

use rustic_git_workspaces::history::alerts::{alert_row, state_of, CATALOGUE};

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
        let sql = (rule.sql)("westeurope-k3s");
        assert!(sql.contains("westeurope-k3s"), "{} ignores its region", rule.name);
        assert!(sql.to_uppercase().starts_with("SELECT"), "{} is not a SELECT", rule.name);
    }
    // The reverse direction: every bolded alert name in the table has a rule here.
    for line in md.lines().filter(|l| l.starts_with("| **")) {
        let name = line.trim_start_matches("| **").split("**").next().unwrap();
        assert!(CATALOGUE.iter().any(|r| r.name == name), "{name} is in deploy/alerts.md but not in CATALOGUE");
    }
}

/// Each rule names the metric it actually reads and the window it reads it over, so a rename in
/// the collector's tables shows up here rather than as a silently permanent `unknown`.
#[test]
fn every_rule_queries_its_own_metric_over_a_window() {
    for rule in CATALOGUE {
        let sql = (rule.sql)("eu");
        assert!(sql.contains("MetricName"), "{} selects no metric", rule.name);
        assert!(sql.contains("INTERVAL"), "{} has no window", rule.name);
        assert!(rule.for_secs >= 30, "{} has a sub-bucket for window", rule.name);
    }
    let by = |n: &str| CATALOGUE.iter().find(|r| r.name == n).expect(n);
    assert!((by("NoLeader").sql)("eu").contains("ownership_is_leader"));
    assert_eq!(by("NoLeader").for_secs, 120);
    // The catalogue gives DbFenceDetected no `for`: one breached bucket fires.
    assert_eq!(by("DbFenceDetected").for_secs, 30);
    assert!((by("PoolAlmostFull").sql)("eu").contains("node_pool_bytes_used"));
    assert!((by("NodeDiskAlmostFull").sql)("eu").contains("k8s.node.filesystem.usage"));
    assert!((by("WorkerHeartbeatStale").sql)("eu").contains("k8s.container.restarts"));
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
