//! User-facing `/v1` workspaces/environments/regions routes, in-process against a mocked API
//! server (`kube_test`) for the cluster half and `MemStore` for the region half.
//!
//! Every mutation's whole output is an object in the API server, so the assertions are about what
//! the handler POSTed or PATCHed, read back off the mock's recorder.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState};
use rustic_git_workspaces::kube_test::{get, mock_client, post, stub_registry, Recorder, Route};
use rustic_git_workspaces::upstream::Upstream;
use rustic_git_workspaces::store::{MemStore, MetaStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

const API: &str = "/apis/rustic-git.io/v1alpha1";
const NODE: &str = "node-a";

struct Server {
    base: String,
    store: Arc<MemStore>,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

fn vol_obj(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner, "rustic-git.io/kind": "workspace"}},
        "spec": {"owner": owner, "nodeName": NODE, "region": "centralindia", "quotaGb": 20}
    })
}

/// A `Workspace` as the API server echoes it back: `spec.storage`, no node and no `volumeRef` —
/// both of those are facts the controllers report in status.
fn ws_obj(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {
            "owner": owner, "team": "", "name": name, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": "running"
        }
    })
}

/// The same, once a node has claimed it and created its Volume.
fn placed_ws(name: &str, owner: &str) -> Value {
    let mut w = ws_obj(name, owner);
    w["status"] = json!({"phase": "ready", "nodeName": NODE, "compatibleNodes": [NODE], "volumeRef": name});
    w
}

/// A freshly created `Environment`: no status, because no controller has seen it yet.
fn new_env(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Environment",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {
            "owner": owner, "name": name, "region": "centralindia", "services": [],
            "storage": {"quotaGb": 20}, "desiredState": "running"
        }
    })
}

fn env_obj(name: &str, owner: &str) -> Value {
    let mut e = json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Environment",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {
            "owner": owner, "name": name, "region": "centralindia", "services": [],
            "storage": {"quotaGb": 20}, "desiredState": "running"
        }
    });
    e["status"] = json!({"phase": "running", "nodeName": NODE, "volumeRef": name});
    e
}

/// The ONE write a create makes now.
fn create_routes() -> Vec<Route> {
    vec![
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik")),
        post(format!("{API}/environments"), new_env("env-new", "karthik")),
    ]
}

async fn server_with(admins: &[&str], routes: Option<Vec<Route>>) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(
        store.clone() as Arc<dyn MetaStore>,
        jwt.clone(),
        admins.iter().map(|s| s.to_string()).collect::<HashSet<_>>(),
    );
    let rec = match routes {
        Some(routes) => {
            let (client, rec) = mock_client(routes);
            state = state.with_kube(client);
            rec
        }
        None => Recorder::default(),
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), store, jwt, rec }
}

async fn server(routes: Vec<Route>) -> Server {
    server_with(&[], Some(routes)).await
}

/// The same, plus a stand-in server tier — needed by every route that reads snapshots, since those
/// records do not live in the cluster.
async fn server_with_registry(routes: Vec<Route>, registry_base: String) -> Server {
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(store.clone() as Arc<dyn MetaStore>, jwt.clone(), HashSet::new())
        .with_kube(client)
        .with_upstream(Arc::new(Upstream::new(registry_base, "peer-secret")));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), store, jwt, rec }
}

fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

async fn region(store: &MemStore, id: &str) {
    store
        .put_region(&rustic_git_workspaces::model::Region {
            id: id.into(),
            name: id.into(),
            storage_account: "acct".into(),
            blob_container: "wslayers".into(),
            status: "active".into(),
            agent_token: format!("tok-{id}"),
        })
        .await
        .unwrap();
}

/// One object per user action. The API used to write two and pick a node; both are the
/// controllers' now, and the node it would have picked is a fact it has no way to know yet.
#[tokio::test]
async fn create_ws_writes_exactly_one_unplaced_workspace() {
    let s = server(create_routes()).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20, "image": "nginx:alpine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());

    let calls = s.rec.calls();
    assert!(!calls.iter().any(|c| c.contains("/volumes")), "the API never creates a Volume: {calls:?}");
    assert!(!calls.iter().any(|c| c.contains("ownerbindings")), "the API never places: {calls:?}");
    assert!(!calls.iter().any(|c| c.contains("/nodes")), "and never reads node capacity: {calls:?}");
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["name"], "web");
    assert_eq!(w["spec"]["desiredState"], "running");
    assert_eq!(w["spec"]["storage"]["quotaGb"], 20);
    assert!(w["spec"]["storage"]["source"].is_null(), "a fresh workspace has no source volume");
    // audit H1, in its controller-ownership form: the Volume's node comes from its parent's
    // status, which is now a controller invariant. The API's half is writing NO node at all —
    // two places allowed to name one is two places that can disagree about where the data is.
    assert!(w["spec"].get("nodeName").is_none(), "placement is a fact the controllers establish: {w}");
    assert!(w["spec"].get("volumeRef").is_none(), "a volumeRef in spec was a wish about a fact: {w}");
    assert_eq!(w["metadata"]["labels"]["rustic-git.io/owner"], "karthik");
}

/// A clone no longer copies a node from the source: locality is the claim's job, via the source's
/// `status.compatibleNodes`.
#[tokio::test]
async fn clone_asks_for_a_clone_source_and_names_no_node() {
    let mut src = placed_ws("ws-src", "karthik");
    src["status"]["nodeName"] = json!("node-z");
    src["status"]["compatibleNodes"] = json!(["node-z"]);
    let s = server(vec![
        get(format!("{API}/workspaces/ws-src"), src),
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik")),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-src/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["storage"]["source"]["cloneOf"]["volume"], "ws-src");
    assert_eq!(w["spec"]["storage"]["quotaGb"], 20, "the copy inherits the source's quota");
    assert!(w["spec"].get("nodeName").is_none(), "{w}");
    assert!(!s.rec.calls().iter().any(|c| c.contains("/volumes")), "a clone reads no Volume");
}

/// A release-1 source has no `spec.storage`, and 0 is not a default anywhere — `k8s::local_pv`
/// formats the quota straight into a `0Gi` PV. The size of a legacy source lives on its Volume,
/// which is the object the controller sizes the disk from.
#[tokio::test]
async fn cloning_a_legacy_source_takes_the_quota_off_its_volume() {
    let mut src = placed_ws("ws-src", "karthik");
    src["spec"].as_object_mut().unwrap().remove("storage");
    let mut vol = vol_obj("ws-src", "karthik");
    vol["spec"]["quotaGb"] = json!(55);
    let s = server(vec![
        get(format!("{API}/workspaces/ws-src"), src),
        get(format!("{API}/volumes/ws-src"), vol),
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik")),
    ])
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-src/clone", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["storage"]["quotaGb"], 55, "never 0: {w}");
}

/// A restore names a SNAPSHOT, and the snapshot is found in the server tier's history — so this
/// works when the source workspace is long gone, which is when a restore is most wanted.
#[tokio::test]
async fn restore_of_a_deleted_workspaces_snapshot_succeeds() {
    let up = stub_registry(
        vec![("karthik", json!([{"name": "ws-gone", "latest_ms": 1i64}]))],
        vec![(
            "karthik/ws-gone",
            json!([{"id": "snap-old", "state": {"kind": "workspace", "name": "api-scratch"},
                    "lineage": [], "region": "centralindia", "created_at": "2026-08-27T09:00:00Z"}]),
        )],
    )
    .await;
    // No `Workspace` named `ws-gone` anywhere: the source was deleted.
    let routes = vec![
        rustic_git_workspaces::kube_test::not_found(format!("{API}/workspaces/ws-gone")),
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik")),
    ];
    let s = server_with_registry(routes, up).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/restore", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web-old", "snapshot_id": "snap-old", "quota_gb": 40}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["storage"]["source"]["restoreOf"]["volume"], "ws-gone", "found by snapshot id: {w}");
    // `rename_all = "camelCase"` on the enum renames VARIANTS, not struct-variant fields — the
    // wire key is the field's own name.
    assert_eq!(w["spec"]["storage"]["source"]["restoreOf"]["snapshot_id"], "snap-old");
    assert_eq!(w["spec"]["storage"]["quotaGb"], 40, "the body's quota, the source being gone: {w}");
    assert_eq!(w["spec"]["region"], "centralindia", "the record knows where its bytes are");
    assert!(w["spec"].get("nodeName").is_none(), "a restore places nothing either: {w}");
}

/// A live source still sizes its own restore, and the old `src_workspace` field is accepted and
/// ignored so a web build from before this change keeps working through a roll.
#[tokio::test]
async fn restore_from_a_live_workspace_takes_its_quota() {
    let up = stub_registry(
        vec![("karthik", json!([{"name": "ws-src", "latest_ms": 1i64}]))],
        vec![(
            "karthik/ws-src",
            json!([{"id": "snap-old", "state": null, "lineage": [], "region": "centralindia",
                    "created_at": "2026-08-27T09:00:00Z"}]),
        )],
    )
    .await;
    let mut src = placed_ws("ws-src", "karthik");
    src["spec"]["storage"]["quotaGb"] = json!(55);
    let routes = vec![
        get(format!("{API}/workspaces/ws-src"), src),
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik")),
    ];
    let s = server_with_registry(routes, up).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/restore", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web-old", "snapshot_id": "snap-old", "src_workspace": "ignored", "quota_gb": 9}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["storage"]["quotaGb"], 55, "the live source's size wins over the body: {w}");
}

/// A snapshot id in nobody's history the caller can read is a 404, and nothing is written — the
/// same answer another owner's snapshot id gets, deliberately indistinguishable.
#[tokio::test]
async fn restore_of_an_unknown_or_foreign_snapshot_is_not_found() {
    let up = stub_registry(
        vec![("karthik", json!([{"name": "ws-mine", "latest_ms": 1i64}])),
             ("alice", json!([{"name": "ws-hers", "latest_ms": 1i64}]))],
        vec![
            ("karthik/ws-mine", json!([{"id": "snap-mine", "state": null, "lineage": [],
                                        "region": "centralindia", "created_at": "2026-08-27T09:00:00Z"}])),
            ("alice/ws-hers", json!([{"id": "snap-hers", "state": null, "lineage": [],
                                      "region": "centralindia", "created_at": "2026-08-27T09:00:00Z"}])),
        ],
    )
    .await;
    let s = server_with_registry(vec![], up).await;
    let tok = token(&s.jwt, "karthik");

    for id in ["nope", "snap-hers"] {
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/workspaces/restore", s.base))
            .bearer_auth(&tok)
            .json(&json!({"name": "web-old", "snapshot_id": id}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "snapshot id {id}");
    }
    assert!(!s.rec.calls().iter().any(|c| c.starts_with("POST")));
}

#[tokio::test]
async fn start_and_stop_patch_the_desired_state() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: placed_ws("ws-1", "karthik") },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");
    let client = reqwest::Client::new();

    for (verb, want) in [("stop", "stopped"), ("start", "running")] {
        let resp = client
            .post(format!("{}/v1/workspaces/ws-1/{verb}", s.base))
            .bearer_auth(&tok)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        let patch = s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1")).pop().unwrap();
        assert_eq!(patch["spec"]["desiredState"], want);
    }
}

/// Delete is ONE call. The "Workspace first, then Volume" ordering became the API server's job the
/// moment the Volume got an ownerReference.
#[tokio::test]
async fn delete_is_one_call() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        Route { method: "DELETE", path: format!("{API}/workspaces/ws-1"), status: 200, body: placed_ws("ws-1", "karthik") },
    ];
    let s = server(routes).await;

    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/workspaces/ws-1", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let deletes: Vec<_> = s.rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(deletes, vec![format!("DELETE {API}/workspaces/ws-1")], "the GC removes the Volume");
}

#[tokio::test]
async fn missing_token_is_unauthorized() {
    let s = server(vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn wrong_token_is_unauthorized() {
    let s = server(vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth("not-a-real-token")
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

/// No cluster configured (dev, or no kubeconfig) — workspace routes answer 503, not a 404 that
/// would read as "this feature doesn't exist".
#[tokio::test]
async fn workspace_routes_without_a_cluster_are_503() {
    let s = server_with(&[], None).await;
    let tok = token(&s.jwt, "karthik");
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);

    let resp = client.get(format!("{}/v1/workspaces", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(resp.status(), 503);

    let resp = client.get(format!("{}/v1/environments", s.base)).bearer_auth(&tok).send().await.unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn region_create_requires_admin() {
    let s = server_with(&["admin@example.com"], None).await;
    let client = reqwest::Client::new();

    let non_admin = token(&s.jwt, "karthik");
    let resp = client
        .post(format!("{}/v1/regions", s.base))
        .bearer_auth(&non_admin)
        .json(&json!({"id": "centralindia", "name": "Central India", "storage_account": "a", "blob_container": "b"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    let admin = token(&s.jwt, "admin");
    let resp = client
        .post(format!("{}/v1/regions", s.base))
        .bearer_auth(&admin)
        .json(&json!({"id": "centralindia", "name": "Central India", "storage_account": "a", "blob_container": "b"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
}

/// A leaked agent token must be revocable. `create_region` preserves an existing token by design,
/// so without this endpoint the only way to invalidate one was editing the store by hand.
#[tokio::test]
async fn rotating_a_region_token_replaces_it_and_is_admin_only() {
    let s = server_with(&["admin@example.com"], None).await;
    let client = reqwest::Client::new();
    let admin = token(&s.jwt, "admin");

    let created: serde_json::Value = client
        .post(format!("{}/v1/regions", s.base))
        .bearer_auth(&admin)
        .json(&json!({"id": "centralindia", "name": "Central India", "storage_account": "a", "blob_container": "b"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first = created["agent_token"].as_str().unwrap().to_string();
    assert!(!first.is_empty(), "a region is created with a token");

    // Re-registering must NOT rotate — that is the behaviour rotate exists to work around.
    let again: serde_json::Value = client
        .post(format!("{}/v1/regions", s.base))
        .bearer_auth(&admin)
        .json(&json!({"id": "centralindia", "name": "Central India", "storage_account": "a", "blob_container": "b"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["agent_token"].as_str().unwrap(), first, "re-register keeps the token");

    // A non-admin cannot rotate somebody's region credential.
    let resp = client
        .post(format!("{}/v1/regions/centralindia/rotate-token", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    let rotated: serde_json::Value = client
        .post(format!("{}/v1/regions/centralindia/rotate-token", s.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second = rotated["agent_token"].as_str().unwrap();
    assert_ne!(second, first, "rotation must actually replace the token");
    assert!(!second.is_empty());

    // Unknown region is a 404, not a silently-created one.
    let resp = client
        .post(format!("{}/v1/regions/nosuch/rotate-token", s.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// Same rule as `create_ws`, on the environment side.
#[tokio::test]
async fn create_env_writes_exactly_one_unplaced_environment() {
    let s = server(create_routes()).await;
    region(&s.store, "centralindia").await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "app-dev", "region": "centralindia", "services": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["state"], "creating", "an object the controller has not seen yet has no status");

    assert!(!s.rec.calls().iter().any(|c| c.contains("/volumes")), "the API never creates a Volume");
    let e = s.rec.sent("POST", &format!("{API}/environments")).remove(0);
    assert_eq!(e["spec"]["name"], "app-dev");
    assert_eq!(e["spec"]["desiredState"], "running");
    assert_eq!(e["metadata"]["labels"]["rustic-git.io/kind"], "environment");
    assert!(e["spec"].get("nodeName").is_none(), "placement is the controllers': {e}");
}

/// The C1 fix: a traversing mount is refused BEFORE anything is written, so a root controller
/// never sees one.
#[tokio::test]
async fn a_traversing_mount_is_refused_before_any_object_is_written() {
    let s = server(create_routes()).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&tok)
        .json(&json!({
            "name": "app-dev", "region": "centralindia",
            "services": [{"name": "web", "image": "nginx", "command": [], "env": {}, "ports": [],
                          "mounts": [{"folder": "/", "path": "/host"}]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(s.rec.calls().is_empty(), "nothing may be written before the mount check");
}

/// The agent work surface (register/work/jobs/{id}/done|failed) lives on the server tier
/// (`bins/server`'s `/vol-agent/*`) — this router never mounted it.
#[tokio::test]
async fn agent_routes_are_gone_from_the_api_router() {
    let s = server(vec![]).await;
    let resp = reqwest::Client::new().post(format!("{}/v1/agent/register", s.base)).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

// ── push ─────────────────────────────────────────────────────────────────

/// A created `SnapshotRequest` as the API server echoes it back.
fn snap_obj() -> serde_json::Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequest",
        "metadata": {"name": "snap-1"},
        "spec": {"volume": "ws-1"},
    })
}

/// Push is still the one mutating verb; the OBJECT is the work item now — a `SnapshotRequest` with
/// somewhere to put the outcome, which the annotation it replaces did not have. The volume it names
/// is the subvolume that gets pushed, and the owner is read off that volume, never off the caller.
#[tokio::test]
async fn push_creates_a_snapshot_request_for_the_volume_with_its_message() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/volumes/ws-1"), vol_obj("ws-1", "karthik")),
        Route { method: "POST", path: format!("{API}/snapshotrequests"), status: 201, body: snap_obj() },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .json(&json!({"message": "checkpoint"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());

    let req = s.rec.sent("POST", &format!("{API}/snapshotrequests")).remove(0);
    assert_eq!(req["spec"]["volume"], "ws-1");
    assert_eq!(req["spec"]["message"], "checkpoint");
    assert_eq!(req["metadata"]["labels"]["rustic-git.io/volume"], "ws-1");
    assert_eq!(req["metadata"]["labels"]["rustic-git.io/owner"], "karthik");
    // Set at creation: the work can start on the very first reconcile, and adding the finalizer
    // afterwards leaves a window where a delete orphans an in-flight `btrfs send`.
    assert_eq!(req["metadata"]["finalizers"][0], "rustic-git.io/snapshot");
}

#[tokio::test]
async fn push_with_no_body_omits_the_message() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/volumes/ws-1"), vol_obj("ws-1", "karthik")),
        Route { method: "POST", path: format!("{API}/snapshotrequests"), status: 201, body: snap_obj() },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let req = s.rec.sent("POST", &format!("{API}/snapshotrequests")).remove(0);
    assert!(req["spec"].get("message").is_none());
}

#[tokio::test]
async fn env_push_targets_the_environments_own_volume() {
    let routes = vec![
        get(format!("{API}/environments/env-1"), env_obj("env-1", "karthik")),
        get(format!("{API}/volumes/env-1"), vol_obj("env-1", "karthik")),
        Route { method: "POST", path: format!("{API}/snapshotrequests"), status: 201, body: snap_obj() },
    ];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/push", s.base))
        .bearer_auth(&tok)
        .json(&json!({"message": "snap"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let req = s.rec.sent("POST", &format!("{API}/snapshotrequests")).remove(0);
    assert_eq!(req["spec"]["volume"], "env-1");
    assert_eq!(req["spec"]["message"], "snap");
}

/// Someone else's workspace is a 404, never a 403 — and no request object is created.
#[tokio::test]
async fn push_on_someone_elses_workspace_is_not_found() {
    let routes = vec![get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "alice"))];
    let s = server(routes).await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(!s.rec.calls().iter().any(|c| c.starts_with("POST")));
}

/// A workspace whose Volume does not exist yet cannot be pushed. 409 "not ready yet", not a 500 and
/// not a silently dropped request.
#[tokio::test]
async fn push_before_the_volume_exists_is_a_conflict() {
    let s = server(vec![get(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik"))]).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert!(!s.rec.calls().iter().any(|c| c.starts_with("POST")), "no request object for a volume-less push");
}

/// The retry the create's 5 s placement wait defers to. Seeded pods REQUIRE the key mount, so a
/// user whose very first workspace outran its namespace has to get the key on some later request —
/// and a list is the one request every client makes.
#[tokio::test]
async fn listing_reinstalls_the_platform_key_when_the_namespace_secret_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = Arc::new(
        rustic_git_storage::store::Store::open(
            Arc::new(object_store::memory::InMemory::new()),
            tmp.path().join("cache"),
            false,
        )
        .await
        .unwrap(),
    );
    keys.rotate_user_key("karthik", "PRIVATE KEY", "SHA256:abc", None).await.unwrap();

    let list = json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [placed_ws("ws-1", "karthik")]
    });
    // No route for the Secret GET: the mock 404s it, which is exactly "the namespace has no key".
    let routes = vec![
        get(format!("{API}/workspaces"), list),
        get(format!("{API}/snapshotrequests"), json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequestList", "metadata": {}, "items": []
        })),
        Route {
            method: "PATCH",
            path: "/api/v1/namespaces/ws-karthik/secrets/user-key".into(),
            status: 200,
            body: json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "user-key"}}),
        },
    ];
    let store = Arc::new(MemStore::new());
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(store as Arc<dyn MetaStore>, jwt.clone(), HashSet::new())
        .with_kube(client)
        .with_keys(keys);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/v1/workspaces"))
        .bearer_auth(token(&jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let calls = rec.calls();
    assert!(
        calls.iter().any(|c| c == "PATCH /api/v1/namespaces/ws-karthik/secrets/user-key"),
        "the absent Secret is re-installed on list: {calls:?}"
    );
}
