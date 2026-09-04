//! The alert catalogue evaluated on canned scrapes — no network, no cluster. The rules are the
//! module's whole value (the HTTP fetch around them is plumbing), and the one that matters most is
//! "a rule we cannot compute says unknown", which is asserted here rather than trusted.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::admin::monitoring::{
    evaluate_fence, evaluate_no_leader, evaluate_ratio, evaluate_tunnels, signal_rows, sum_of,
    ScrapeSample,
};
use rustic_git_workspaces::api::{admin::router, ApiState};
use rustic_git_workspaces::kube_test::{get, mock_client};
use serde_json::json;
use std::sync::Arc;

const SCRAPE: &str = "\
# HELP ownership_is_leader whether this pod holds the lease
# TYPE ownership_is_leader gauge
ownership_is_leader 1
some_other_metric 5
http_requests_total{listener=\"public\",status=\"2xx\"} 90
http_requests_total{listener=\"public\",status=\"5xx\"} 10
";

#[test]
fn parses_a_named_gauge_out_of_prometheus_text() {
    assert_eq!(sum_of("ownership_is_leader", None, SCRAPE), Some(1.0));
    assert_eq!(sum_of("http_requests_total", None, SCRAPE), Some(100.0));
    assert_eq!(sum_of("http_requests_total", Some(("status", "5xx")), SCRAPE), Some(10.0));
    // Absent is not zero: a metric nobody emits must not read as a healthy zero.
    assert_eq!(sum_of("db_fence_detected_total", None, SCRAPE), None);
}

/// Untrusted text: a scrape body is whatever the pod sent, and none of it may panic.
#[test]
fn a_malformed_scrape_yields_nothing_rather_than_panicking() {
    let junk = "ownership_is_leader\nownership_is_leader NaNnn\n{}}{\n\u{0}\nownership_is_leader ";
    assert_eq!(sum_of("ownership_is_leader", None, junk), None);
}

#[test]
fn no_leader_fires_on_zero_or_two() {
    assert_eq!(evaluate_no_leader(0.0), "firing");
    assert_eq!(evaluate_no_leader(1.0), "ok");
    assert_eq!(evaluate_no_leader(2.0), "firing");
}

#[test]
fn fence_fires_only_on_a_rise() {
    assert_eq!(evaluate_fence(0.0, 0.0), "ok");
    assert_eq!(evaluate_fence(0.0, 1.0), "firing");
    // A counter that went backwards is a restart, not a healthy zero.
    assert_eq!(evaluate_fence(3.0, 1.0), "unknown");
}

#[test]
fn ratio_is_none_without_traffic_in_the_window() {
    assert_eq!(evaluate_ratio((0.0, 10.0), (0.0, 100.0)), Some(0.1));
    assert_eq!(evaluate_ratio((5.0, 5.0), (5.0, 5.0)), None);
}

#[test]
fn tunnels_fire_past_the_catalogue_threshold() {
    assert_eq!(evaluate_tunnels(800.0), "ok");
    assert_eq!(evaluate_tunnels(801.0), "firing");
}

fn state_of(rows: &[rustic_git_workspaces::api::admin::monitoring::SignalRow], alert: &str) -> String {
    rows.iter().find(|r| r.alert == alert).unwrap_or_else(|| panic!("{alert} missing")).state.to_string()
}

/// A rule needing a sustained window is unknown, never guessed as ok — and so is every rule whose
/// metric no pod answered with.
#[test]
fn window_only_rules_report_unknown() {
    let rows = signal_rows(&ScrapeSample::empty(), None);
    for alert in [
        "LeaseRenewFailing",
        "MisdirectedWrites",
        "WorkerHeartbeatStale",
        "PoolAlmostFull",
        "NodeDiskAlmostFull",
        // Nothing was scraped, so these have no observation either.
        "NoLeader",
        "DbFenceDetected",
        "Http5xxRate",
        "ReconcileErrors",
        "TunnelSaturation",
    ] {
        assert_eq!(state_of(&rows, alert), "unknown", "{alert}");
    }
    // Every catalogue rule, and only those.
    assert_eq!(rows.len(), 10);
}

/// The firing/ok half, driven from a canned pair of scrapes rather than a cluster.
#[test]
fn rules_fire_on_canned_samples() {
    let before = {
        let mut s = ScrapeSample::empty();
        s.absorb("db_fence_detected_total 0\nhttp_requests_total{status=\"5xx\"} 0\nhttp_requests_total{status=\"2xx\"} 0\nreconciles_total{result=\"error\"} 0\nreconciles_total{result=\"ok\"} 0\n");
        s.sample
    };
    let mut now = ScrapeSample::empty();
    now.absorb(
        "ownership_is_leader 1\ngateway_open_tunnels 900\ndb_fence_detected_total 1\nhttp_requests_total{status=\"5xx\"} 10\nhttp_requests_total{status=\"2xx\"} 90\nreconciles_total{result=\"error\"} 30\nreconciles_total{result=\"ok\"} 70\n",
    );
    let rows = signal_rows(&now, Some(&before));
    assert_eq!(state_of(&rows, "NoLeader"), "ok");
    assert_eq!(state_of(&rows, "DbFenceDetected"), "firing");
    assert_eq!(state_of(&rows, "Http5xxRate"), "firing"); // 10% > 5%
    assert_eq!(state_of(&rows, "ReconcileErrors"), "firing"); // 30% > 20%
    assert_eq!(state_of(&rows, "TunnelSaturation"), "firing");

    // The healthy fleet, same shape.
    let mut healthy = ScrapeSample::empty();
    healthy.absorb("ownership_is_leader 1\ngateway_open_tunnels 3\ndb_fence_detected_total 0\nhttp_requests_total{status=\"5xx\"} 0\nhttp_requests_total{status=\"2xx\"} 100\nreconciles_total{result=\"error\"} 0\nreconciles_total{result=\"ok\"} 100\n");
    let rows = signal_rows(&healthy, Some(&before));
    for alert in ["NoLeader", "DbFenceDetected", "Http5xxRate", "ReconcileErrors", "TunnelSaturation"] {
        assert_eq!(state_of(&rows, alert), "ok", "{alert}");
    }
}

/// A pod that cannot be scraped never 5xxes the page: the handler answers 200 with the rules
/// `unknown` and the pod named among the failures.
#[tokio::test]
async fn a_pod_that_cannot_be_scraped_is_unknown_not_an_error() {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let pods = json!({
        "apiVersion": "v1", "kind": "PodList", "metadata": {},
        "items": [{
            "apiVersion": "v1", "kind": "Pod",
            // Annotated for scraping but with no pod IP yet — unreachable without any network.
            "metadata": {"name": "rustic-git-srv-0", "namespace": "rustic-git",
                         "annotations": {"prometheus.io/scrape": "true"}},
            "status": {"containerStatuses": [{"name": "server", "restartCount": 4,
                                              "image": "x", "imageID": "x", "ready": true}]},
        }],
    });
    let (client, _rec) = mock_client(vec![get("/api/v1/namespaces/rustic-git/pods", pods)]);
    let state = ApiState::new(jwt.clone()).with_aks(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/admin/monitoring/signals"))
        .bearer_auth(jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["signals"].as_array().unwrap().len(), 10);
    assert!(body["signals"].as_array().unwrap().iter().all(|r| r["state"] == "unknown"));
    assert_eq!(body["scrape_failures"][0][0], "rustic-git-srv-0");
    let srv = body["restarts"].as_array().unwrap().iter().find(|r| r["workload"] == "rustic-git-srv").unwrap();
    assert_eq!(srv["restarts"], 4);
    // No RUSTIC_GIT_GRAFANA_URL in the test environment: no dead link on the page.
    assert!(body.get("grafana_url").is_none());
}
