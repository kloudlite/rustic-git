//! `/v1/bench` against a mocked API server and a stub directory: the person-only access rules,
//! the team's region, the wake, and the bench's share of the person's quota.

mod common;
use common::{admin_token_as, token};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState, Directory, TeamRole};
use kloudlite_workspaces::crd::bench_id;
use kloudlite_workspaces::kube_test::{get, mock_client, not_found, patch, post, Recorder, Route};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const API: &str = "/apis/kloudlite.io/v1alpha1";

#[derive(Default)]
struct Stub {
    /// (person, team) memberships.
    members: Vec<(&'static str, &'static str)>,
    /// Slug → bound region; "" = known and unbound, absent = no such owner.
    regions: Mutex<HashMap<String, String>>,
    binds: Mutex<Vec<(String, String)>>,
}

impl Stub {
    fn new(members: &[(&'static str, &'static str)], regions: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Stub {
            members: members.to_vec(),
            regions: Mutex::new(regions.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
            binds: Mutex::default(),
        })
    }
}

#[async_trait::async_trait]
impl Directory for Stub {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        self.members.iter().filter(|(u, _)| *u == user).map(|(_, t)| t.to_string()).collect()
    }
    async fn is_live(&self, _jti: &str) -> bool {
        false
    }
    async fn for_owner(&self, _owner: &str) -> Option<kloudlite_workspaces::api::OwnerMaterial> {
        None
    }
    async fn authorized_keys_for_owner(&self, _owner: &str) -> Option<String> {
        None
    }
    async fn owners_of(&self, _email: &str) -> Vec<String> {
        Vec::new()
    }
    async fn team_role(&self, _user: &str, _team: &str) -> Option<TeamRole> {
        None
    }
    async fn is_team(&self, slug: &str) -> bool {
        slug == "acme"
    }
    async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
        Err("no directory".into())
    }
    async fn add_superadmin(&self, _e: &str, _b: &str) -> Result<(), String> {
        Ok(())
    }
    async fn region_of(&self, slug: &str) -> Option<String> {
        self.regions.lock().unwrap().get(slug).filter(|r| !r.is_empty()).cloned()
    }
    async fn bind_region(&self, slug: &str, region: &str) -> Result<Option<String>, String> {
        self.binds.lock().unwrap().push((slug.into(), region.into()));
        let mut m = self.regions.lock().unwrap();
        let Some(slot) = m.get_mut(slug) else { return Ok(None) };
        if slot.is_empty() {
            *slot = region.to_string();
        }
        Ok(Some(slot.clone()))
    }
}

struct T {
    state: Arc<ApiState>,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

fn setup(routes: Vec<Route>, dir: Arc<Stub>) -> T {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = Arc::new(ApiState::new(jwt.clone()).with_kube(client).with_directory(dir));
    T { state, jwt, rec }
}

impl T {
    async fn call(&self, method: &str, uri: &str, tok: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {tok}"))
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = router(self.state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, v)
    }
    fn tok(&self, who: &str) -> String {
        token(&self.jwt, who)
    }
    fn bench_writes(&self) -> Vec<String> {
        self.rec.calls().into_iter().filter(|c| c.contains("/benches") && !c.starts_with("GET")).collect()
    }
}

fn bench_path(owner: &str, team: &str) -> String {
    format!("{API}/benches/{}", bench_id(owner, team))
}

fn bench_obj(owner: &str, team: &str, desired: &str, phase: Option<&str>, access: &str) -> Value {
    let mut b = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Bench",
        "metadata": {"name": bench_id(owner, team)},
        "spec": {"owner": owner, "team": team, "image": "i", "desiredState": desired, "access": access,
                 "resources": {"cpuRequest": "1", "cpuLimit": "1", "memoryRequest": "1Gi", "memoryLimit": "1Gi"}}
    });
    if let Some(p) = phase {
        b["status"] = json!({"phase": p, "nodeName": "node-a", "idleSince": "2026-09-13T10:00:00Z"});
    }
    b
}

fn list(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn region(id: &str) -> Route {
    get(format!("{API}/regions/{id}"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Region", "metadata": {"name": id}, "spec": {"name": id, "status": "active"}}))
}

/// What `guard_alloc` reads for `owner`: no Quota objects (the compiled-in default) and empty listings.
fn alloc(owner: &str, benches: Vec<Value>) -> Vec<Route> {
    vec![
        not_found(format!("{API}/quotas/{owner}")),
        not_found(format!("{API}/quotas/default-user")),
        get(format!("{API}/workspaces"), list("Workspace", vec![])),
        get(format!("{API}/environments"), list("Environment", vec![])),
        get(format!("{API}/volumes"), list("Volume", vec![])),
        get(format!("{API}/snapshots"), list("Snapshot", vec![])),
        get(format!("{API}/benches"), list("Bench", benches)),
    ]
}

fn with(mut a: Vec<Route>, b: Vec<Route>) -> Vec<Route> {
    a.extend(b);
    a
}

#[tokio::test]
async fn creating_a_bench_twice_is_one_object_and_starts_it() {
    let path = bench_path("alice", "acme");
    let t = setup(
        with(
            vec![
                not_found(path.clone()),
                get(path.clone(), bench_obj("alice", "acme", "stopped", None, "full")),
                post(format!("{API}/benches"), bench_obj("alice", "acme", "running", None, "full")),
                patch(path.clone(), bench_obj("alice", "acme", "running", None, "full")),
                region("r1"),
            ],
            alloc("alice", vec![]),
        ),
        Stub::new(&[("alice", "acme")], &[("acme", "r1")]),
    );
    let tok = t.tok("alice");
    let (st, _) = t.call("POST", "/v1/bench", &tok, Some(json!({"team": "acme"}))).await;
    assert_eq!(st, 201);
    assert_eq!(t.rec.sent("POST", &format!("{API}/benches")).len(), 1);
    let (st, doc) = t.call("POST", "/v1/bench", &tok, Some(json!({"team": "acme"}))).await;
    assert_eq!(st, 200, "{doc}");
    assert_eq!(t.rec.sent("POST", &format!("{API}/benches")).len(), 1, "no second create");
    let patches = t.rec.sent("PATCH", &path);
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0]["spec"]["desiredState"], "running");
}

#[tokio::test]
async fn a_non_member_without_a_bench_cannot_see_create_or_tunnel_to_a_teams_bench() {
    let t = setup(
        with(vec![not_found(bench_path("bob", "acme")), region("r1")], alloc("bob", vec![])),
        Stub::new(&[], &[("acme", "r1")]),
    );
    let tok = t.tok("bob");
    for (m, uri, body) in [
        ("GET", "/v1/bench?team=acme", None),
        ("POST", "/v1/bench", Some(json!({"team": "acme"}))),
        ("POST", "/v1/bench/start?team=acme", None),
        ("POST", "/v1/bench/session?team=acme", None),
    ] {
        let (st, _) = t.call(m, uri, &tok, body).await;
        assert_eq!(st, 404, "{m} {uri}");
    }
    assert!(t.bench_writes().is_empty(), "{:?}", t.bench_writes());
}

#[tokio::test]
async fn a_departed_member_reads_their_own_bench_and_nothing_more() {
    let path = bench_path("alice", "acme");
    let idle = bench_obj("alice", "acme", "running", Some("idle"), "full");
    let t = setup(
        with(vec![get(path.clone(), idle.clone()), patch(path.clone(), idle.clone()), region("r1")], alloc("alice", vec![])),
        Stub::new(&[], &[("acme", "r1")]),
    );
    let tok = t.tok("alice");
    let (st, _) = t.call("GET", "/v1/bench?team=acme", &tok, None).await;
    assert_eq!(st, 200);
    assert_eq!(t.rec.sent("PATCH", &path)[0]["spec"]["access"], "readOnly");
    let (st, body) = t.call("POST", "/v1/bench/session?team=acme", &tok, None).await;
    assert_eq!((st, body), (StatusCode::ACCEPTED, json!({"state": "waking"})));
    assert!(t.rec.sent("PATCH", &path).iter().any(|p| p["spec"]["wakeAt"].is_string()));
    let (st, _) = t.call("POST", "/v1/bench", &tok, Some(json!({"team": "acme"}))).await;
    assert_eq!(st, 404);
    let (st, _) = t.call("POST", "/v1/bench/attach?team=acme", &tok, Some(json!({"environment": "env-1"}))).await;
    assert_eq!(st, 404);

    let ready = bench_obj("alice", "acme", "running", Some("ready"), "readOnly");
    let t = setup(vec![get(path.clone(), ready), region("r1")], Stub::new(&[], &[("acme", "r1")]));
    let (st, _) = t.call("POST", "/v1/bench/session?team=acme", &t.tok("alice"), None).await;
    assert_eq!(st, 201);

    let carol = bench_obj("carol", "acme", "running", Some("ready"), "full");
    let dave = bench_obj("dave", "dave", "running", Some("ready"), "full");
    let t = setup(
        vec![
            get(format!("{API}/benches"), list("Bench", vec![carol.clone(), dave.clone()])),
            patch(bench_path("carol", "acme"), carol),
            patch(bench_path("dave", "dave"), dave),
        ],
        Stub::new(&[], &[]),
    );
    kloudlite_workspaces::api::keys::readonly_departed_benches(&t.state).await;
    let p = t.rec.sent("PATCH", &bench_path("carol", "acme"));
    assert_eq!(p.len(), 1);
    assert_eq!(p[0]["spec"]["access"], "readOnly");
    assert!(t.rec.sent("PATCH", &bench_path("dave", "dave")).is_empty());
}

#[tokio::test]
async fn the_region_comes_from_the_team_and_a_person_binds_their_own_once() {
    let routes = || {
        with(
            vec![
                not_found(bench_path("alice", "acme")),
                not_found(bench_path("alice", "alice")),
                post(format!("{API}/benches"), bench_obj("alice", "acme", "running", None, "full")),
                region("r1"),
                region("r2"),
            ],
            alloc("alice", vec![]),
        )
    };
    let t = setup(routes(), Stub::new(&[("alice", "acme")], &[("acme", "")]));
    let (st, body) = t.call("POST", "/v1/bench", &t.tok("alice"), Some(json!({"team": "acme"}))).await;
    assert_eq!(st, 409);
    assert_eq!(body["error"], "team acme has no region; a platform admin binds one");

    let dir = Stub::new(&[("alice", "acme")], &[("acme", "r1"), ("alice", "")]);
    let t = setup(routes(), dir.clone());
    let tok = t.tok("alice");
    let (st, body) = t.call("POST", "/v1/bench", &tok, Some(json!({"team": "acme", "region": "r2"}))).await;
    assert_eq!(st, 409);
    assert_eq!(body["error"], "team acme is in region r1");
    let (st, _) = t.call("POST", "/v1/bench", &tok, Some(json!({"team": "acme"}))).await;
    assert_eq!(st, 201);
    let created = t.rec.sent("POST", &format!("{API}/benches")).pop().unwrap();
    assert!(!created["spec"].to_string().contains("region"), "{created}");

    let (st, body) = t.call("POST", "/v1/bench", &tok, Some(json!({}))).await;
    assert_eq!(st, 422);
    assert_eq!(body["error"], "choose a region for your personal bench");
    let (st, _) = t.call("POST", "/v1/bench", &tok, Some(json!({"region": "r2"}))).await;
    assert_eq!(st, 201);
    assert!(dir.binds.lock().unwrap().contains(&("alice".into(), "r2".into())));

    let id = bench_id("alice", "acme");
    let t = setup(
        vec![get(bench_path("alice", "acme"), bench_obj("alice", "acme", "running", Some("ready"), "full"))],
        Stub::new(&[("alice", "acme")], &[("acme", "r1")]),
    );
    let (st, body) = t.call("POST", "/v1/bench/session?team=acme", &t.tok("alice"), None).await;
    assert_eq!(st, 201);
    let claims = t.jwt.verify_bench_session(body["token"].as_str().unwrap()).unwrap();
    assert_eq!(claims.region, "r1");
    assert_eq!(body["gateway"], format!("wss://ws-r1.khost.dev/tunnel/{id}"));
}

#[tokio::test]
async fn a_superadmin_claim_does_not_open_someone_elses_bench() {
    let bobs = bench_obj("bob", "acme", "running", Some("ready"), "full");
    let t = setup(
        vec![get(bench_path("bob", "acme"), bobs.clone()), get(bench_path("root", "acme"), bobs), region("r1")],
        Stub::new(&[("bob", "acme")], &[("acme", "r1")]),
    );
    let tok = admin_token_as(&t.jwt, "root");
    for (m, uri) in [("GET", "/v1/bench?team=acme"), ("POST", "/v1/bench/session?team=acme")] {
        let (st, _) = t.call(m, uri, &tok, None).await;
        assert_eq!(st, 404, "{m} {uri}");
    }
}

#[tokio::test]
async fn a_session_wakes_an_idle_bench_waits_on_a_starting_one_and_refuses_a_stopped_one() {
    let path = bench_path("alice", "alice");
    let dir = || Stub::new(&[], &[("alice", "r1")]);
    let seeded = |b: Value, extra: Vec<Route>| setup(with(vec![get(path.clone(), b.clone()), patch(path.clone(), b)], extra), dir());

    let t = seeded(bench_obj("alice", "alice", "running", Some("idle"), "full"), alloc("alice", vec![]));
    let (st, body) = t.call("POST", "/v1/bench/session", &t.tok("alice"), None).await;
    assert_eq!((st, body), (StatusCode::ACCEPTED, json!({"state": "waking"})));
    let p = t.rec.sent("PATCH", &path);
    assert_eq!(p.len(), 1);
    let at = chrono::DateTime::parse_from_rfc3339(p[0]["spec"]["wakeAt"].as_str().unwrap()).unwrap();
    assert!((chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds().abs() <= 5);

    let t = seeded(bench_obj("alice", "alice", "running", Some("starting"), "full"), vec![]);
    let (st, body) = t.call("POST", "/v1/bench/session", &t.tok("alice"), None).await;
    assert_eq!((st, body), (StatusCode::ACCEPTED, json!({"state": "starting"})));
    assert!(t.rec.sent("PATCH", &path).is_empty());

    let t = seeded(bench_obj("alice", "alice", "running", Some("ready"), "full"), vec![]);
    let (st, body) = t.call("POST", "/v1/bench/session", &t.tok("alice"), None).await;
    assert_eq!(st, 201);
    let claims = t.jwt.verify_bench_session(body["token"].as_str().unwrap()).unwrap();
    assert_eq!((claims.bench.as_str(), claims.region.as_str()), (bench_id("alice", "alice").as_str(), "r1"));

    let t = seeded(bench_obj("alice", "alice", "stopped", Some("idle"), "full"), vec![]);
    let (st, body) = t.call("POST", "/v1/bench/session", &t.tok("alice"), None).await;
    assert_eq!(st, 409);
    assert_eq!(body["error"], "bench is stopped; start it");
    assert!(t.rec.sent("PATCH", &path).is_empty());

    let mut full = alloc("alice", vec![]);
    full[0] = get(
        format!("{API}/quotas/alice"),
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota", "metadata": {"name": "alice"},
               "spec": {"workspaces": 5, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 0, "memoryGb": 100}}),
    );
    let t = seeded(bench_obj("alice", "alice", "running", Some("idle"), "full"), full);
    let (st, body) = t.call("POST", "/v1/bench/session", &t.tok("alice"), None).await;
    assert_eq!(st, 409);
    assert_eq!(body, Value::String(kloudlite_workspaces::quota::refuse(kloudlite_workspaces::quota::Dim::Cpu, 0, 0)));
    assert!(t.rec.sent("PATCH", &path).is_empty());
}

#[tokio::test]
async fn a_bench_costs_the_person_only_while_it_has_a_pod() {
    let usage = |b: Value| async move {
        let (client, _) = mock_client(alloc("alice", vec![b]));
        let alice = kloudlite_workspaces::quota::usage(&client, "alice").await.unwrap();
        let acme = kloudlite_workspaces::quota::usage(&client, "acme").await.unwrap();
        (alice.cpu, acme.cpu)
    };
    assert_eq!(usage(bench_obj("alice", "acme", "running", Some("ready"), "full")).await, (1, 0));
    assert_eq!(usage(bench_obj("alice", "acme", "running", Some("idle"), "full")).await, (0, 0));
    assert_eq!(usage(bench_obj("alice", "acme", "stopped", Some("ready"), "full")).await, (0, 0));
}

#[test]
fn the_admin_router_has_no_bench_route() {
    let mut srcs = vec![include_str!("../src/api/admin.rs").to_string()];
    for e in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src/api/admin")).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            srcs.push(std::fs::read_to_string(p).unwrap());
        }
    }
    for src in srcs {
        assert!(!src.contains("/bench"), "no platform surface reads a person's bench");
    }
}
