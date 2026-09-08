//! `POST /v1/environments/{id}/intercepts` writes one workspace's wish to receive a service's
//! traffic, and `DELETE …/intercepts/{service}` is the ONLY thing that takes it away. In-process
//! against the same mocked API server `api_packages.rs` uses.
//!
//! The rule the last test pins: stopping the workspace leaves the wish in spec. The controller
//! brings the real service back; the person's ask survives the night.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState};
use kloudlite_workspaces::kube_test::{get, mock_client, Recorder, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/kloudlite.io/v1alpha1";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

fn empty(kind: &str, plural: &str) -> Route {
    get(
        format!("{API}/{plural}"),
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": []}),
    )
}

fn env_obj(intercepts: Value) -> Value {
    json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
        "metadata": {"name": "env-1", "labels": {"kloudlite.io/owner": "karthik"}},
        "spec": {
            "owner": "karthik", "name": "app", "region": "centralindia",
            "services": [{"name": "api", "image": "nginx", "command": [], "env": {}, "mounts": [], "ports": [8080, 9090]}],
            "storage": {"quotaGb": 20}, "desiredState": "running",
            "intercepts": intercepts,
        },
        "status": {"phase": "ready", "nodeName": "node-a", "volumeRef": "env-1"},
    })
}

fn ws_obj(name: &str, owner: &str, attached: Option<&str>, state: &str) -> Value {
    json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "labels": {"kloudlite.io/owner": owner}},
        "spec": {
            "owner": owner, "team": "", "name": name, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": state,
            "attachedEnvironment": attached,
        },
        "status": {"phase": "ready", "nodeName": "node-a", "volumeRef": name},
    })
}

/// The reads every intercept call makes: the environment, the workspace, and the snapshot list
/// `env_doc`'s pushed set comes from. The PATCH answers with the environment unchanged — what was
/// SENT is what these tests assert on.
fn routes(intercepts: Value, ws: Value) -> Vec<Route> {
    vec![
        get(format!("{API}/environments/env-1"), env_obj(intercepts.clone())),
        get(format!("{API}/workspaces/ws-1"), ws),
        empty("Snapshot", "snapshots"),
        Route { method: "PATCH", path: format!("{API}/environments/env-1"), status: 200, body: env_obj(intercepts) },
    ]
}

async fn server(routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(jwt.clone()).with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn token(jwt: &Jwt) -> String {
    jwt.mint("karthik@example.com", "Test User", Some("karthik")).unwrap()
}

async fn intercept(s: &Server, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/intercepts", s.base))
        .bearer_auth(token(&s.jwt))
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn good() -> Value {
    json!({"service": "api", "workspace": "ws-1", "ports": [{"service": 8080, "workspace": 3000}]})
}

fn attached_running() -> Value {
    ws_obj("ws-1", "karthik", Some("env-1"), "running")
}

fn patched(s: &Server) -> Vec<Value> {
    s.rec.sent("PATCH", &format!("{API}/environments/env-1"))
}

#[tokio::test]
async fn a_service_the_environment_does_not_have_is_404() {
    let s = server(routes(json!([]), attached_running())).await;
    let r = intercept(&s, json!({"service": "nope", "workspace": "ws-1"})).await;
    assert_eq!(r.status(), 404);
    assert!(patched(&s).is_empty(), "nothing written");
}

#[tokio::test]
async fn someone_elses_workspace_is_404() {
    let s = server(routes(json!([]), ws_obj("ws-1", "other", Some("env-1"), "running"))).await;
    assert_eq!(intercept(&s, good()).await.status(), 404);
    assert!(patched(&s).is_empty(), "nothing written");
}

#[tokio::test]
async fn a_workspace_attached_elsewhere_is_409() {
    let s = server(routes(json!([]), ws_obj("ws-1", "karthik", Some("env-2"), "running"))).await;
    let r = intercept(&s, good()).await;
    assert_eq!(r.status(), 409);
    let body = r.text().await.unwrap();
    assert!(body.contains("attached"), "{body}");
}

#[tokio::test]
async fn a_stopped_workspace_is_409() {
    let s = server(routes(json!([]), ws_obj("ws-1", "karthik", Some("env-1"), "stopped"))).await;
    assert_eq!(intercept(&s, good()).await.status(), 409);
    assert!(patched(&s).is_empty(), "nothing written");
}

#[tokio::test]
async fn a_service_another_workspace_holds_is_409_naming_it() {
    let held = json!([{"service": "api", "workspace": "ws-other", "ports": []}]);
    let s = server(routes(held, attached_running())).await;
    let r = intercept(&s, good()).await;
    assert_eq!(r.status(), 409);
    let body = r.text().await.unwrap();
    assert!(body.contains("ws-other"), "the holder is named: {body}");
}

#[tokio::test]
async fn a_port_the_service_does_not_declare_is_422() {
    let s = server(routes(json!([]), attached_running())).await;
    let r = intercept(&s, json!({"service": "api", "workspace": "ws-1", "ports": [{"service": 1234, "workspace": 3000}]})).await;
    assert_eq!(r.status(), 422);
    assert!(patched(&s).is_empty(), "nothing written");
}

#[tokio::test]
async fn the_same_service_port_twice_is_422() {
    let s = server(routes(json!([]), attached_running())).await;
    let r = intercept(
        &s,
        json!({"service": "api", "workspace": "ws-1",
               "ports": [{"service": 8080, "workspace": 3000}, {"service": 8080, "workspace": 4000}]}),
    )
    .await;
    assert_eq!(r.status(), 422);
    assert!(patched(&s).is_empty(), "nothing written");
}

#[tokio::test]
async fn a_good_request_writes_one_entry_with_the_mapping() {
    let s = server(routes(json!([]), attached_running())).await;
    let r = intercept(&s, good()).await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let p = patched(&s).pop().unwrap();
    assert_eq!(
        p,
        json!({"spec": {"intercepts": [{"service": "api", "workspace": "ws-1", "ports": [{"service": 8080, "workspace": 3000}]}]}})
    );
}

#[tokio::test]
async fn the_same_workspace_asking_again_replaces_rather_than_duplicates() {
    let held = json!([{"service": "api", "workspace": "ws-1", "ports": [{"service": 8080, "workspace": 1111}]}]);
    let s = server(routes(held, attached_running())).await;
    assert_eq!(intercept(&s, good()).await.status(), 202);
    let p = patched(&s).pop().unwrap();
    let list = p["spec"]["intercepts"].as_array().unwrap();
    assert_eq!(list.len(), 1, "one entry per service: {p}");
    assert_eq!(list[0]["ports"][0]["workspace"], 3000, "the new mapping won");
}

async fn release(s: &Server, service: &str) -> reqwest::Response {
    reqwest::Client::new()
        .delete(format!("{}/v1/environments/env-1/intercepts/{service}", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_release_removes_the_entry() {
    let held = json!([{"service": "api", "workspace": "ws-1", "ports": []}]);
    let s = server(routes(held, attached_running())).await;
    assert_eq!(release(&s, "api").await.status(), 204);
    let p = patched(&s).pop().unwrap();
    assert_eq!(p, json!({"spec": {"intercepts": []}}));
}

#[tokio::test]
async fn releasing_one_that_is_not_there_is_still_204() {
    let s = server(routes(json!([]), attached_running())).await;
    assert_eq!(release(&s, "api").await.status(), 204);
    assert!(patched(&s).is_empty(), "an idempotent release writes nothing");
}

/// The rule this whole feature turns on: a stop is not a release. The wish stays in spec and the
/// agent brings the real service back; only `DELETE …/intercepts/{service}` takes it away.
#[tokio::test]
async fn stopping_the_workspace_leaves_the_intercept_alone() {
    let held = json!([{"service": "api", "workspace": "ws-1", "ports": []}]);
    let mut rs = routes(held.clone(), attached_running());
    rs.push(Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: attached_running() });
    let s = server(rs).await;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/stop", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    assert!(patched(&s).is_empty(), "a stop must not touch spec.intercepts");
    // …and the environment still reports the wish, which is what the web renders as "held, not in
    // force" rather than as gone.
    let e = reqwest::Client::new()
        .get(format!("{}/v1/environments/env-1", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap();
    let doc: Value = e.json().await.unwrap();
    assert_eq!(doc["intercepts"], held, "{doc}");
}
