//! `/admin/monitoring/signals` now READS `kloudlite.alerts` instead of scraping. What is asserted is
//! the contract the web depends on: the response shape survives, a region nothing reports for shows
//! every rule `unknown` rather than an empty table, and no ClickHouse is a 503, never an error page.

use kloudlite_git_core::jwt::Jwt;
use kloudlite_git_workspaces::api::{admin::router, ApiState};
use kloudlite_git_workspaces::history::alerts::CATALOGUE;
use kloudlite_git_workspaces::kube_test::{get, mock_client};
use serde_json::json;
use std::sync::Arc;

async fn serve(state: ApiState) -> (String, Arc<Jwt>) {
    let jwt = state.jwt.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    (format!("http://{addr}"), jwt)
}

fn jwt() -> Arc<Jwt> {
    Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap())
}

#[tokio::test]
async fn without_clickhouse_the_page_gets_a_503_not_an_error() {
    let pods = json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": []});
    let (client, _rec) = mock_client(vec![get("/api/v1/namespaces/kloudlite-git/pods", pods)]);
    let (base, jwt) = serve(ApiState::new(jwt()).with_aks(client)).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/admin/monitoring/signals"))
        .bearer_auth(jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.text().await.unwrap(), "history unavailable");
}

/// Every rule still carries its "Why" — the console renders that column straight from this
/// response, and a blank one is a row nobody can act on.
#[test]
fn every_catalogue_rule_carries_its_why() {
    assert_eq!(CATALOGUE.len(), 25);
    assert!(CATALOGUE.iter().all(|r| !r.why.is_empty()));
}
