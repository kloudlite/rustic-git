//! `GET/PUT /admin/settings/central` and `/admin/settings/clusters/{region}` against a mocked
//! kube API (and, for the central scope, a mocked server-tier peer route) — Task 6 Step 4.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{admin::router, admin::PeerClient, ApiState};
use kloudlite_workspaces::kube_test::{get, mock_client, patch, Recorder, Route};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const API: &str = "/apis/kloudlite.io/v1alpha1";

fn jwt() -> Arc<Jwt> {
    Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap())
}

fn admin_token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

async fn keys_store() -> Arc<kloudlite_storage::store::Store> {
    let tmp = tempfile::tempdir().unwrap();
    Arc::new(
        kloudlite_storage::store::Store::open(
            Arc::new(object_store::memory::InMemory::new()),
            tmp.path().join("cache"),
            false,
        )
        .await
        .unwrap(),
    )
}

/// A `keys_store` with `cluster/settings` pre-seeded — `revert_central` reads `history[0]`
/// straight off the object store (`current_central`), same as `get_central` does, so a test that
/// wants a non-empty history writes the document here rather than going through a PUT first.
async fn keys_store_with(doc: &Value) -> Arc<kloudlite_storage::store::Store> {
    let store = keys_store().await;
    let key = slatedb::object_store::path::Path::from(kloudlite_core::settings::CENTRAL_SETTINGS_KEY);
    slatedb::object_store::ObjectStoreExt::put(
        store.os.as_ref(),
        &key,
        slatedb::object_store::PutPayload::from(serde_json::to_vec(doc).unwrap()),
    )
    .await
    .unwrap();
    store
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

async fn admin_server(routes: Vec<Route>, keys: Option<Arc<kloudlite_storage::store::Store>>, peer: Option<PeerClient>) -> Server {
    let jwt = jwt();
    let mut state = ApiState::new(jwt.clone());
    let (client, rec) = mock_client(routes);
    state = state.with_kube(client);
    if let Some(k) = keys {
        state = state.with_keys(k);
    }
    if let Some(p) = peer {
        state = state.with_peer(p);
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

/// A minimal stand-in for `bins/server/src/router/admin_settings.rs`'s `PUT /api/admin/settings`
/// — just enough to prove the admin route forwards the peer secret and the caller's bearer token,
/// and returns whatever body this fake server answers with. Records every request it sees.
struct PeerMock {
    base: String,
    secret: String,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}

async fn peer_mock(secret: &str, respond: Value, status: u16) -> PeerMock {
    use axum::{extract::State, http::HeaderMap, routing::post, routing::put, Json, Router};
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    #[derive(Clone)]
    struct S {
        seen: Arc<Mutex<Vec<(String, Value)>>>,
        respond: Value,
        status: u16,
    }
    async fn handler(State(s): State<S>, headers: HeaderMap, Json(body): Json<Value>) -> axum::response::Response {
        use axum::response::IntoResponse;
        let peer_hdr = headers.get(kloudlite_core::peer::PEER_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        s.seen.lock().unwrap().push((peer_hdr, body));
        (axum::http::StatusCode::from_u16(s.status).unwrap(), Json(s.respond)).into_response()
    }
    // `revert_central` sends no body, so this mock's revert arm accepts an empty one too —
    // `Json<Value>` with an empty request body is a null `Value`, matched the same way.
    async fn revert_handler(State(s): State<S>, headers: HeaderMap, body: axum::body::Bytes) -> axum::response::Response {
        use axum::response::IntoResponse;
        let peer_hdr = headers.get(kloudlite_core::peer::PEER_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body: Value = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or_default() };
        s.seen.lock().unwrap().push((peer_hdr, body));
        (axum::http::StatusCode::from_u16(s.status).unwrap(), Json(s.respond)).into_response()
    }
    let state = S { seen: seen2, respond, status };
    let app = Router::new()
        .route("/api/admin/settings", put(handler))
        .route("/api/admin/settings/revert", post(revert_handler))
        .with_state(state);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    PeerMock { base: format!("http://{addr}"), secret: secret.to_string(), seen }
}

// ── central ──────────────────────────────────────────────────────────────

/// An out-of-range field is a 422 named by `core::settings::validate_stored`, decided locally —
/// nothing is forwarded to the server tier at all, so `cluster/settings` is never even asked
/// about (spec §7's "the settings write is NOT made", the strongest form of "422 propagates").
#[tokio::test]
async fn put_central_out_of_range_is_422_and_forwards_nothing() {
    let peer = peer_mock("shh", json!({}), 200).await;
    let s = admin_server(vec![], Some(keys_store().await), Some(PeerClient::new(peer.base.clone(), peer.secret.clone()))).await;
    let resp = reqwest::Client::new()
        .put(format!("{}/admin/settings/central", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"sshPort": 0, "note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 422);
    let body = resp.text().await.unwrap();
    assert!(body.contains("ssh_port"), "{body}");
    assert!(peer.seen.lock().unwrap().is_empty(), "the server tier must never be called on a 422");
}

/// A valid write forwards the patch (with the peer secret and the caller's bearer token) to the
/// server tier's peer route, and returns whatever that route answers with — the history array
/// included, proving the round trip rather than the admin process re-deriving it.
#[tokio::test]
async fn put_central_forwards_and_returns_the_peer_routes_body() {
    let history_doc = json!({"maxBody": 999999, "history": [{"maxBody": 111}], "updatedBy": "root@example.com"});
    let peer = peer_mock("shh", history_doc.clone(), 200).await;
    let s = admin_server(vec![], Some(keys_store().await), Some(PeerClient::new(peer.base.clone(), peer.secret.clone()))).await;
    let resp = reqwest::Client::new()
        .put(format!("{}/admin/settings/central", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"maxBody": 4194304, "note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let body: Value = reqwest::Client::new()
        .put(format!("{}/admin/settings/central", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"maxBody": 4194304, "note": "test"}))
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(body, history_doc);
    let seen = peer.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, "shh", "the peer secret must ride along");
    assert_eq!(seen[0].1["maxBody"], 4194304);
}

/// No history yet (a fresh `cluster/settings` document): 422 decided locally, same as the
/// range-error case above — the server tier is never even asked.
#[tokio::test]
async fn revert_central_with_no_history_is_422_and_forwards_nothing() {
    let peer = peer_mock("shh", json!({}), 200).await;
    let s = admin_server(vec![], Some(keys_store().await), Some(PeerClient::new(peer.base.clone(), peer.secret.clone()))).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/settings/central/revert", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 422);
    assert!(peer.seen.lock().unwrap().is_empty(), "the server tier must never be called with no history to revert to");
}

/// A document with history forwards a bare `POST` (with the peer secret and the caller's bearer
/// token, no body) to the server tier's revert route, and returns whatever that route answers
/// with — same round-trip shape as the PUT twin above.
#[tokio::test]
async fn revert_central_forwards_and_returns_the_peer_routes_body() {
    let current_doc = json!({"maxBody": 8388608, "history": [{"maxBody": 4194304}], "updatedBy": "root@example.com"});
    let reverted_doc = json!({"maxBody": 4194304, "history": [{"maxBody": 8388608}], "updatedBy": "root@example.com"});
    let peer = peer_mock("shh", reverted_doc.clone(), 200).await;
    let s = admin_server(
        vec![],
        Some(keys_store_with(&current_doc).await),
        Some(PeerClient::new(peer.base.clone(), peer.secret.clone())),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/settings/central/revert", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, reverted_doc);
    let seen = peer.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "shh", "the peer secret must ride along");
}

// ── cluster ──────────────────────────────────────────────────────────────

fn region(id: &str) -> Value {
    json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Region", "metadata": {"name": id}, "spec": {"name": id, "status": "active"}})
}

fn cluster_settings(annotations: Value) -> Value {
    json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "ClusterSettings",
        "metadata": {"name": "default", "annotations": annotations},
        "spec": {"syncSecs": 60},
    })
}

/// `caller.name` is the USERNAME claim (`mint_admin`'s third argument, `"root"`), not the email —
/// same rule every owner-scoped route in this crate enforces.
const CALLER: &str = "root";

/// Out of range: 422, and the recorder shows no PATCH was ever sent — the CR is untouched.
#[tokio::test]
async fn put_cluster_out_of_range_is_422_and_writes_nothing() {
    let s = admin_server(
        vec![get(format!("{API}/regions/us"), region("us")), get(format!("{API}/clustersettings/default"), cluster_settings(json!({})))],
        None,
        None,
    )
    .await;
    let resp = reqwest::Client::new()
        .put(format!("{}/admin/settings/clusters/us", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"syncSecs": 1, "note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 422);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("PATCH")));
}

/// A LIVE field (not `Mark::Boot`) writes the annotation trail and never asks about a reader's
/// rollout — `syncSecs` has no reader entry in `CLUSTER_SETTING_META` at all.
#[tokio::test]
async fn put_cluster_writes_annotations_and_history() {
    let updated = cluster_settings(json!({"kloudlite.io/updated-by": CALLER}));
    let s = admin_server(
        vec![
            get(format!("{API}/regions/us"), region("us")),
            get(format!("{API}/clustersettings/default"), cluster_settings(json!({}))),
            patch(format!("{API}/clustersettings/default"), updated),
        ],
        None,
        None,
    )
    .await;
    let resp = reqwest::Client::new()
        .put(format!("{}/admin/settings/clusters/us", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"syncSecs": 120, "note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let sent = s.rec.sent("PATCH", &format!("{API}/clustersettings/default"));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["spec"]["syncSecs"], 120);
    assert_eq!(sent[0]["metadata"]["annotations"]["kloudlite.io/updated-by"], CALLER);
    let hist = &sent[0]["metadata"]["annotations"]["kloudlite.io/settings-history"];
    assert!(hist.as_str().unwrap().contains("syncSecs"), "the OLD spec must be pushed onto history");
}

/// A `Mark::Boot` field (`defaultImage` → `kloudlite-agent`) mid-rollout is a 409, `{ready,
/// desired}`, and the CR is not touched — precheck-before-write for the cluster scope, same shape
/// `roll_readers` already proves for the manual roll route.
#[tokio::test]
async fn put_cluster_boot_field_conflicts_mid_rollout_and_writes_nothing() {
    let s = admin_server(
        vec![
            get(format!("{API}/regions/us"), region("us")),
            get(format!("{API}/clustersettings/default"), cluster_settings(json!({}))),
            get(
                "/apis/apps/v1/namespaces/kube-system/daemonsets/kloudlite-agent",
                json!({
                    "apiVersion": "apps/v1", "kind": "DaemonSet",
                    "metadata": {"name": "kloudlite-agent", "namespace": "kube-system"},
                    "spec": {"template": {"metadata": {"annotations": {}}, "spec": {"containers": [{"name": "c", "image": "img:1"}]}}},
                    "status": {"numberReady": 1, "desiredNumberScheduled": 2},
                }),
            ),
        ],
        None,
        None,
    )
    .await;
    let resp = reqwest::Client::new()
        .put(format!("{}/admin/settings/clusters/us", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"defaultImage": "img:2", "note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ready"], 1);
    assert_eq!(body["desired"], 2);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("PATCH")));
}

/// Revert restores the named history entry's fields and pushes a NEW history entry for the
/// restore itself — a revert is a write, not a rewind (constraints.md, Step 3).
#[tokio::test]
async fn revert_cluster_restores_the_named_entry_and_grows_history_again() {
    let hist = json!([{"syncSecs": 30}]);
    let current = cluster_settings(json!({"kloudlite.io/settings-history": hist.to_string()}));
    let s = admin_server(
        vec![
            get(format!("{API}/regions/us"), region("us")),
            get(format!("{API}/clustersettings/default"), current),
            patch(format!("{API}/clustersettings/default"), cluster_settings(json!({}))),
        ],
        None,
        None,
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/admin/settings/clusters/us/revert/0", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let sent = s.rec.sent("PATCH", &format!("{API}/clustersettings/default"));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["spec"]["syncSecs"], 30, "the reverted spec must carry the historical value");
    let new_hist: Value = serde_json::from_str(sent[0]["metadata"]["annotations"]["kloudlite.io/settings-history"].as_str().unwrap()).unwrap();
    assert_eq!(new_hist[0]["syncSecs"], 60, "the CURRENT spec (pre-revert) is pushed onto history by the revert write");
}

// ── boot vs live field rolls (fix round 2) ──────────────────────────────

fn daemonset(ready: i32, desired: i32) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "DaemonSet",
        "metadata": {"name": "kloudlite-agent", "namespace": "kube-system"},
        "spec": {"template": {"metadata": {"annotations": {}}, "spec": {"containers": [{"name": "c", "image": "img:1"}]}}},
        "status": {"numberReady": ready, "desiredNumberScheduled": desired},
    })
}

const AGENT_DS: &str = "/apis/apps/v1/namespaces/kube-system/daemonsets/kloudlite-agent";
const GATEWAY_DEPLOY: &str = "/apis/apps/v1/namespaces/kloudlite-system/deployments/kloudlite-gateway";

/// A `Mark::Boot` field (`defaultImage`) whose one reader is settled: the CR is written AND
/// `kloudlite-agent`'s DaemonSet is PATCHed with a fresh `kloudlite.io/restarted-at` — and
/// nothing else is (the per-region gateway is never a reader of this field).
#[tokio::test]
async fn put_cluster_boot_field_rolls_only_its_reader() {
    let s = admin_server(
        vec![
            get(format!("{API}/regions/us"), region("us")),
            get(format!("{API}/clustersettings/default"), cluster_settings(json!({}))),
            get(AGENT_DS, daemonset(2, 2)),
            patch(format!("{API}/clustersettings/default"), cluster_settings(json!({}))),
            patch(AGENT_DS, daemonset(2, 2)),
        ],
        None,
        None,
    )
    .await;
    let resp = reqwest::Client::new()
        .put(format!("{}/admin/settings/clusters/us", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"defaultImage": "img:2", "note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);

    assert_eq!(s.rec.sent("PATCH", &format!("{API}/clustersettings/default")).len(), 1, "the CR must be written");
    let ds_patches = s.rec.sent("PATCH", AGENT_DS);
    assert_eq!(ds_patches.len(), 1, "the one reader must be rolled exactly once");
    assert!(ds_patches[0]["spec"]["template"]["metadata"]["annotations"]["kloudlite.io/restarted-at"].is_string());
    assert!(s.rec.calls().iter().all(|c| !c.contains(GATEWAY_DEPLOY)), "the gateway is not a reader of defaultImage");
}

/// A LIVE field alone (`syncSecs`) writes the CR and rolls nothing — no PATCH to any workload at
/// all, `kloudlite-agent` included.
#[tokio::test]
async fn put_cluster_live_field_rolls_nothing() {
    let s = admin_server(
        vec![
            get(format!("{API}/regions/us"), region("us")),
            get(format!("{API}/clustersettings/default"), cluster_settings(json!({}))),
            patch(format!("{API}/clustersettings/default"), cluster_settings(json!({}))),
        ],
        None,
        None,
    )
    .await;
    let resp = reqwest::Client::new()
        .put(format!("{}/admin/settings/clusters/us", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"syncSecs": 90, "note": "test"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    assert_eq!(s.rec.sent("PATCH", &format!("{API}/clustersettings/default")).len(), 1);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with(&format!("PATCH {AGENT_DS}"))), "a live field must roll no workload");
}

/// An unregistered region is a 404 on BOTH verbs, and neither ever reaches `clustersettings` at
/// all — `client_for_region` refuses before the CR is even asked about.
#[tokio::test]
async fn cluster_settings_unknown_region_is_404_on_get_and_put() {
    let s = admin_server(vec![], None, None).await;

    let get_resp = reqwest::Client::new()
        .get(format!("{}/admin/settings/clusters/nope", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap();
    assert_eq!(get_resp.status(), 404);

    let put_resp = reqwest::Client::new()
        .put(format!("{}/admin/settings/clusters/nope", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"syncSecs": 90, "note": "test"}))
        .send().await.unwrap();
    assert_eq!(put_resp.status(), 404);

    assert!(s.rec.calls().iter().all(|c| !c.contains("clustersettings")), "the CR must never be asked about for an unknown region");
}
