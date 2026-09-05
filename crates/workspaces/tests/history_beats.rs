//! The hourly folds. Usage is recomputed from the CRDs on every run and never derived from an
//! earlier row (CLAUDE.md), so what is tested here is the ROW SHAPE — six dimensions per owner,
//! every one carrying both the used value and the limit it was measured against.

use kloudlite_workspaces::crd::QuotaSpec;
use kloudlite_workspaces::history::beats::{fleet_rows, usage_rows, FleetInput, UsageInput};
use kloudlite_workspaces::quota::Usage;

fn ts() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-09-04T10:00:00Z").unwrap().into()
}

#[test]
fn one_row_per_owner_per_dimension_with_its_limit() {
    let rows = usage_rows(
        ts(),
        &[UsageInput {
            owner: "acme".into(),
            is_team: true,
            used: Usage { workspaces: 3, environments: 1, snapshots: 9, disk_gb: 120, cpu: 6, memory_gb: 24 },
            limit: QuotaSpec { workspaces: 10, environments: 4, snapshots: 50, disk_gb: 500, cpu: 16, memory_gb: 64, regions: Vec::new() },
        }],
    );
    assert_eq!(rows.len(), 6, "six dimensions, one row each");
    assert_eq!(rows[0]["ts"], serde_json::json!("2026-09-04 10:00:00"));
    let ws = rows.iter().find(|r| r["dimension"] == "workspaces").unwrap();
    assert_eq!(ws["owner"], serde_json::json!("acme"));
    // A team is `1`, a person `0`: the column is UInt8, not a Bool.
    assert_eq!(ws["is_team"], serde_json::json!(1));
    assert_eq!(ws["used"], serde_json::json!(3.0));
    assert_eq!(ws["limit"], serde_json::json!(10.0));
    // The dimension words are `Dim::word`'s, which the 409 message and the request form already
    // key off — a second vocabulary here would silently split every chart in two.
    let mut dims: Vec<&str> = rows.iter().map(|r| r["dimension"].as_str().unwrap()).collect();
    dims.sort_unstable();
    assert_eq!(dims, ["cpu", "diskGb", "environments", "memoryGb", "snapshots", "workspaces"]);
}

#[test]
fn a_fleet_row_carries_every_column_the_table_declares() {
    let rows = fleet_rows(
        ts(),
        &[FleetInput {
            region: "westeurope-k3s".into(),
            nodes_total: 3,
            nodes_ready: 2,
            agents_ready: 2,
            live_workspaces: 7,
            live_environments: 2,
            snapshots: 41,
            disk_gb: 900,
            cpu: 24,
            memory_gb: 96,
            pool_used_bytes: 500_000_000_000,
            pool_total_bytes: 1_000_000_000_000,
        }],
    );
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    for col in [
        "ts",
        "region",
        "nodes_total",
        "nodes_ready",
        "agents_ready",
        "live_workspaces",
        "live_environments",
        "snapshots",
        "disk_gb",
        "cpu",
        "memory_gb",
        "pool_used_bytes",
        "pool_total_bytes",
    ] {
        assert!(r.get(col).is_some(), "fleet row is missing {col}");
    }
    assert_eq!(r["nodes_ready"], serde_json::json!(2));
    assert_eq!(r["pool_total_bytes"], serde_json::json!(1_000_000_000_000u64));
}

/// No owners is a legitimate hour (a brand-new cluster), and it must produce no rows rather than
/// a row of zeros that a chart would draw as a cliff.
#[test]
fn an_empty_fold_writes_nothing() {
    assert!(usage_rows(ts(), &[]).is_empty());
    assert!(fleet_rows(ts(), &[]).is_empty());
}

// ── one tick, end to end: mocked kube API in, canned ClickHouse out ────────

mod tick {
    use axum::{routing::post, Router};
    use kloudlite_core::jwt::Jwt;
    use kloudlite_workspaces::api::ApiState;
    use kloudlite_workspaces::history::{beats::tick_once, History};
    use kloudlite_workspaces::kube_test::{get, mock_client, Route};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    const API: &str = "/apis/kloudlite.io/v1alpha1";

    /// A canned ClickHouse: records the body of every `INSERT`, answers 200 to all of them.
    async fn canned_clickhouse() -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let app = Router::new().route(
            "/",
            post(move |body: String| {
                let s = s.clone();
                async move {
                    s.lock().unwrap().push(body);
                    (axum::http::StatusCode::OK, "")
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn list_of(kind: &str, items: Vec<Value>) -> Value {
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
    }

    fn ws_obj() -> Value {
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
               "metadata": {"name": "w1"},
               "spec": {"owner": "acme", "team": "", "name": "w1", "region": "r1", "image": "img:1",
                        "desiredState": "running", "packages": [],
                        "resources": {"cpuRequest": "1", "cpuLimit": "2", "memoryRequest": "1Gi", "memoryLimit": "2Gi"},
                        "storage": {"quotaGb": 20}},
               "status": {"phase": "ready", "nodeName": "n1", "podRef": "pod-w1", "volumeRef": "v1"}})
    }

    fn region_obj() -> Value {
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Region",
               "metadata": {"name": "r1"}, "spec": {"name": "Region one", "status": "active"}})
    }

    fn node_obj() -> Value {
        json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n1", "labels": {}, "annotations": {}},
               "status": {"conditions": [{"type": "Ready", "status": "True",
                                          "lastHeartbeatTime": "2026-09-04T00:00:00Z",
                                          "lastTransitionTime": "2026-09-04T00:00:00Z"}]}})
    }

    fn agent_ds() -> Value {
        json!({"apiVersion": "apps/v1", "kind": "DaemonSet",
               "metadata": {"name": "kloudlite-agent", "namespace": "kube-system"},
               "spec": {"selector": {}, "template": {"metadata": {}, "spec": {"containers": [{"name": "agent", "image": "agent:1"}]}}},
               "status": {"numberReady": 1, "desiredNumberScheduled": 1, "currentNumberScheduled": 1,
                          "numberMisscheduled": 0, "numberUnavailable": 0}})
    }

    /// One `tick_once` against a mocked kube API (one owner, one region, one live workspace) and a
    /// canned ClickHouse: both `usage_hourly` and `fleet_hourly` get exactly one INSERT, and each
    /// body carries the row the fold computed — proof the loop wires the folds to the client the
    /// same way the unit tests already proved the row shape.
    #[tokio::test]
    async fn one_tick_inserts_a_usage_row_and_a_fleet_row() {
        let (ch_url, ch_seen) = canned_clickhouse().await;
        let history = Arc::new(History::new(&ch_url, "default", ""));

        let routes: Vec<Route> = vec![
            // `owners::fleet`
            get(format!("{API}/quotas"), list_of("Quota", vec![])),
            get(format!("{API}/quotarequests"), list_of("QuotaRequest", vec![])),
            get(format!("{API}/workspaces"), list_of("Workspace", vec![ws_obj()])),
            get(format!("{API}/environments"), list_of("Environment", vec![])),
            get(format!("{API}/volumes"), list_of("Volume", vec![])),
            get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
            // `clusters::cluster_rows`
            get(format!("{API}/regions"), list_of("Region", vec![region_obj()])),
            get(format!("{API}/regions/r1"), region_obj()),
            get("/api/v1/nodes", json!({"apiVersion": "v1", "kind": "NodeList", "metadata": {}, "items": [node_obj()]})),
            get("/apis/apps/v1/namespaces/kube-system/daemonsets/kloudlite-agent", agent_ds()),
            get(format!("{API}/clustersettings/default"), json!({"apiVersion": "kloudlite.io/v1alpha1",
                "kind": "ClusterSettings", "metadata": {"name": "default"}, "spec": {}, "status": {}})),
        ];
        let (client, _rec) = mock_client(routes);

        let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
        let state = Arc::new(ApiState::new(jwt).with_kube(client).with_history(history));

        tick_once(&state).await;

        let bodies = ch_seen.lock().unwrap();
        let usage: Vec<&String> = bodies.iter().filter(|b| b.starts_with("INSERT INTO kloudlite.usage_hourly")).collect();
        let fleet: Vec<&String> = bodies.iter().filter(|b| b.starts_with("INSERT INTO kloudlite.fleet_hourly")).collect();
        assert_eq!(usage.len(), 1, "one usage_hourly insert: {bodies:?}");
        assert_eq!(fleet.len(), 1, "one fleet_hourly insert: {bodies:?}");

        // Six dimension rows for the one owner the mock listed a workspace for.
        let usage_rows: Vec<&str> = usage[0].lines().skip(1).collect();
        assert_eq!(usage_rows.len(), 6, "{}", usage[0]);
        assert!(usage_rows.iter().any(|r| r.contains(r#""owner":"acme""#) && r.contains(r#""dimension":"workspaces""#)));

        // One region row, carrying the live workspace the mock listed.
        let fleet_rows: Vec<&str> = fleet[0].lines().skip(1).collect();
        assert_eq!(fleet_rows.len(), 1, "{}", fleet[0]);
        assert!(fleet_rows[0].contains(r#""region":"r1""#));
        assert!(fleet_rows[0].contains(r#""live_workspaces":1"#));
    }
}
