//! `/v1` locks every `name@version` entry BEFORE it writes the CR — on create, patch, clone and
//! restore — and `POST /v1/workspaces/{id}/packages/update` re-resolves what a workspace already
//! has. In-process against the same mocked API server `api_user.rs` uses, with a fake index
//! standing in for Nixhub and the mirror.
//!
//! The two facts every test here is about: a refusal writes NOTHING, and an entry whose string
//! did not change is never asked about again.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState};
use kloudlite_workspaces::crd::{Lock, LockSource};
use kloudlite_workspaces::kube_test::{get, mock_client, post, Recorder, Route};
use kloudlite_workspaces::packages::resolve::{BinaryCache, Index, Resolver};
use kloudlite_workspaces::packages::VersionReq;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const API: &str = "/apis/kloudlite.io/v1alpha1";

/// Every version the fake publishes, newest first — the same shape a real index answers with.
#[derive(Default)]
struct FakeState {
    down: bool,
    calls: usize,
    /// Bumped to make an update route's answer differ from what the workspace already had.
    patch: u32,
}

struct FakeIndex(Arc<Mutex<FakeState>>);

impl FakeIndex {
    fn versions_of(&self, attr: &str) -> Vec<String> {
        let patch = self.0.lock().unwrap().patch;
        match attr {
            "nodejs" => vec![format!("20.20.{patch}"), "20.19.0".into(), "18.4.0".into()],
            "python3" => vec!["3.11.9".into(), "3.10.1".into()],
            _ => vec![],
        }
    }
}

#[async_trait::async_trait]
impl Index for FakeIndex {
    async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String> {
        {
            let mut st = self.0.lock().unwrap();
            st.calls += 1;
            if st.down {
                return Err("index down".into());
            }
        }
        let want = match version {
            VersionReq::Latest => String::new(),
            VersionReq::Prefix(p) => p.clone(),
        };
        let Some(v) = self.versions_of(attr).into_iter().find(|v| v.starts_with(&want)) else {
            return Ok(None);
        };
        Ok(Some(Lock {
            entry: format!("{attr}@{want}"),
            version: v,
            attr_path: attr.to_string(),
            rev: "c0ffee".repeat(6) + "abcd",
            store_path: "/nix/store/aaa".into(),
            resolved_at: String::new(),
            source: LockSource::Nixhub,
        }))
    }

    async fn versions(&self, attr: &str) -> Result<Vec<String>, String> {
        if self.0.lock().unwrap().down {
            return Err("index down".into());
        }
        Ok(self.versions_of(attr))
    }
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
    fake: Arc<Mutex<FakeState>>,
}

fn region_obj() -> Route {
    get(
        format!("{API}/regions/centralindia"),
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Region",
               "metadata": {"name": "centralindia"}, "spec": {"name": "centralindia", "status": "active"}}),
    )
}

fn empty(kind: &str, plural: &str) -> Route {
    get(
        format!("{API}/{plural}"),
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": []}),
    )
}

/// The quota gate's reads, with no `Quota` object anywhere so the compiled-in default applies.
fn base_routes() -> Vec<Route> {
    vec![
        region_obj(),
        empty("Workspace", "workspaces"),
        empty("Environment", "environments"),
        empty("Snapshot", "snapshots"),
        empty("Volume", "volumes"),
        kloudlite_workspaces::kube_test::not_found(format!("{API}/quotas/karthik")),
        kloudlite_workspaces::kube_test::not_found(format!("{API}/quotas/default-user")),
        post(format!("{API}/workspaces"), ws_obj("ws-new", &[], &[])),
    ]
}

fn lock_json(entry: &str, version: &str) -> Value {
    json!({"entry": entry, "version": version, "attrPath": "nodejs", "rev": "deadbeef",
           "storePath": "/nix/store/old", "resolvedAt": "2026-01-01T00:00:00Z", "source": "nixhub"})
}

fn ws_obj(name: &str, packages: &[&str], locks: &[Value]) -> Value {
    json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "labels": {"kloudlite.io/owner": "karthik"}},
        "spec": {
            "owner": "karthik", "team": "", "name": name, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": "running",
            "packages": packages, "locks": locks,
        },
        "status": {"phase": "ready", "nodeName": "node-a", "volumeRef": name},
    })
}

/// `with_index: false` is the dev deployment that has no package index wired at all.
/// The cache holds whatever the fake index names: cache misses are the resolver's own tests.
struct EveryPathCached;
#[async_trait::async_trait]
impl BinaryCache for EveryPathCached {
    async fn has(&self, _: &str) -> Result<bool, String> {
        Ok(true)
    }
}

async fn server_with(routes: Vec<Route>, with_index: bool) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let fake = Arc::new(Mutex::new(FakeState::default()));
    let mut state = ApiState::new(jwt.clone()).with_kube(client);
    if with_index {
        state = state.with_resolver(Arc::new(Resolver {
            cache: Arc::new(slatedb::object_store::memory::InMemory::new()),
            nixhub: Arc::new(FakeIndex(fake.clone())),
            // One fake for both slots: the mirror is only ever reached when Nixhub says "no", and
            // an index that agrees with itself keeps every assertion here about the API's own
            // behaviour rather than about which of the two answered.
            mirror: Arc::new(FakeIndex(fake.clone())),
            binaries: Arc::new(EveryPathCached),
            now: chrono::Utc::now,
        }));
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec, fake }
}

async fn server(routes: Vec<Route>) -> Server {
    server_with(routes, true).await
}

fn token(jwt: &Jwt) -> String {
    jwt.mint("karthik@example.com", "Test User", Some("karthik")).unwrap()
}

async fn create(s: &Server, packages: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt))
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20, "packages": packages}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn create_locks_the_pinned_entry_and_leaves_the_bare_one_alone() {
    let s = server(base_routes()).await;
    let r = create(&s, json!(["jq", "nodejs@20"])).await;
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["packages"], json!(["jq", "nodejs@20"]));
    let locks = w["spec"]["locks"].as_array().unwrap();
    assert_eq!(locks.len(), 1, "only the pinned entry is locked: {w}");
    assert_eq!(locks[0]["entry"], "nodejs@20");
    assert_eq!(locks[0]["version"], "20.20.0");
}

#[tokio::test]
async fn a_list_with_no_pin_never_asks_the_index() {
    let s = server(base_routes()).await;
    assert_eq!(create(&s, json!(["jq", "hello"])).await.status(), 202);
    assert_eq!(s.fake.lock().unwrap().calls, 0);
}

#[tokio::test]
async fn an_unknown_version_is_422_and_names_the_nearest() {
    let s = server(base_routes()).await;
    let r = create(&s, json!(["nodejs@0.0.99"])).await;
    assert_eq!(r.status(), 422);
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body["error"],
        "nodejs@0.0.99 is not a version anyone published; nearest: 18.4.0, 20.19.0, 20.20.0"
    );
    assert!(s.rec.sent("POST", &format!("{API}/workspaces")).is_empty(), "nothing written");
}

#[tokio::test]
async fn an_index_outage_refuses_a_new_pin_and_writes_nothing() {
    let s = server(base_routes()).await;
    s.fake.lock().unwrap().down = true;
    let r = create(&s, json!(["nodejs@20"])).await;
    assert_eq!(r.status(), 503);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "the package index is unavailable; try again");
    assert!(s.rec.sent("POST", &format!("{API}/workspaces")).is_empty(), "nothing written");
}

/// A dev deployment with no index wired refuses a pin rather than writing an unlocked one — the
/// agent would have nothing to build it from.
#[tokio::test]
async fn a_pin_with_no_resolver_is_503() {
    let s = server_with(base_routes(), false).await;
    let r = create(&s, json!(["nodejs@20"])).await;
    assert_eq!(r.status(), 503);
    assert!(s.rec.sent("POST", &format!("{API}/workspaces")).is_empty(), "nothing written");
    // …and the same deployment still creates an unpinned workspace exactly as before.
    assert_eq!(create(&s, json!(["jq"])).await.status(), 202);
}

fn patch_route(name: &str, packages: &[&str], locks: &[Value]) -> Route {
    Route {
        method: "PATCH",
        path: format!("{API}/workspaces/{name}"),
        status: 200,
        body: ws_obj(name, packages, locks),
    }
}

#[tokio::test]
async fn a_patch_resolves_only_the_entry_that_changed() {
    let held = lock_json("nodejs@20", "20.19.0");
    let routes = vec![
        empty("Snapshot", "snapshots"),
        get(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", &["jq", "nodejs@20"], std::slice::from_ref(&held))),
        patch_route("ws-1", &["jq", "nodejs@20", "python3@3.11"], &[]),
    ];
    let s = server(routes).await;
    let r = reqwest::Client::new()
        .patch(format!("{}/v1/workspaces/ws-1", s.base))
        .bearer_auth(token(&s.jwt))
        .json(&json!({"packages": ["jq", "nodejs@20", "python3@3.11"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert_eq!(s.fake.lock().unwrap().calls, 1, "the untouched entry keeps its lock");
    let p = s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1")).pop().unwrap();
    let locks = p["spec"]["locks"].as_array().unwrap();
    assert_eq!(locks.len(), 2);
    // Carried over verbatim, old version and all: editing one entry must not move another's.
    assert_eq!(locks[0], held);
    assert_eq!(locks[1]["entry"], "python3@3.11");
    assert_eq!(locks[1]["version"], "3.11.9");
}

#[tokio::test]
async fn the_update_route_re_resolves_what_the_workspace_already_has() {
    let held = lock_json("nodejs@20", "20.19.0");
    let fresh = lock_json("nodejs@20", "20.20.7");
    let routes = vec![
        empty("Snapshot", "snapshots"),
        get(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", &["nodejs@20"], &[held])),
        patch_route("ws-1", &["nodejs@20"], &[fresh]),
    ];
    let s = server(routes).await;
    s.fake.lock().unwrap().patch = 7;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/packages/update", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let doc: Value = r.json().await.unwrap();
    assert_eq!(doc["locks"][0]["version"], "20.20.7");
    let p = s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1")).pop().unwrap();
    // The declared list is untouched — only what the pins point at moves.
    assert_eq!(p, json!({"spec": {"locks": p["spec"]["locks"]}}));
    assert_eq!(p["spec"]["locks"][0]["version"], "20.20.7");
}

/// An outage during an update keeps the versions the workspace had rather than taking them away.
#[tokio::test]
async fn an_update_during_an_outage_keeps_the_locks_it_had() {
    let held = lock_json("nodejs@20", "20.19.0");
    let routes = vec![
        empty("Snapshot", "snapshots"),
        get(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", &["nodejs@20"], std::slice::from_ref(&held))),
        patch_route("ws-1", &["nodejs@20"], std::slice::from_ref(&held)),
    ];
    let s = server(routes).await;
    s.fake.lock().unwrap().down = true;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/packages/update", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let p = s.rec.sent("PATCH", &format!("{API}/workspaces/ws-1")).pop().unwrap();
    assert_eq!(p["spec"]["locks"], json!([held]));
}

#[tokio::test]
async fn a_clone_carries_the_sources_locks_without_asking_the_index() {
    let held = lock_json("nodejs@20", "20.19.0");
    let mut routes = base_routes();
    routes.push(get(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", &["nodejs@20"], std::slice::from_ref(&held))));
    routes.push(post(format!("{API}/snapshots"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": "cut"}, "spec": {"owner": "karthik", "volume": "ws-1", "worktree": "ws-1", "parent": "", "transient": true}})));
    let s = server(routes).await;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/clone", s.base))
        .bearer_auth(token(&s.jwt))
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["locks"], json!([held]));
    assert_eq!(s.fake.lock().unwrap().calls, 0, "a clone re-picks no versions");
}

#[tokio::test]
async fn a_restore_carries_the_frozen_locks_and_resolves_only_what_the_body_added() {
    let held = lock_json("nodejs@20", "20.19.0");
    let mut routes = base_routes();
    routes.push(get(
        format!("{API}/snapshots/snap-1"),
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
               "metadata": {"name": "snap-1"},
               "spec": {"owner": "karthik", "volume": "vol-1", "worktree": "ws-src", "parent": "", "transient": false,
                        "state": {"kind": "workspace", "image": "alpine:3.19", "quotaGb": 7,
                                  "resources": {"cpuRequest": "2", "cpuLimit": "4", "memoryRequest": "4Gi", "memoryLimit": "8Gi"},
                                  "packages": ["nodejs@20"], "locks": [held.clone()]}},
               "status": {"phase": "ready"}}),
    ));
    let s = server(routes).await;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/restore", s.base))
        .bearer_auth(token(&s.jwt))
        .json(&json!({"name": "back", "snapshot_id": "snap-1", "packages": ["nodejs@20", "python3@3.11"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202, "{}", r.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    let locks = w["spec"]["locks"].as_array().unwrap();
    assert_eq!(locks[0], held, "the frozen lock is what a restore restores");
    assert_eq!(locks[1]["entry"], "python3@3.11");
    assert_eq!(s.fake.lock().unwrap().calls, 1);
}
