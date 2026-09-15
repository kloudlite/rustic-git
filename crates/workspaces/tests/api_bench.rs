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
/// The one CLI jti the stub directory calls live.
const LIVE_PARENT: &str = "parent-live";
/// The one the stub calls revoked; every other jti (a real `mint_cli`'s) is live.
const REVOKED_PARENT: &str = "parent-revoked";

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
    async fn is_live(&self, jti: &str) -> bool {
        jti != REVOKED_PARENT
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
    async fn membership(&self, team: &str, user: &str) -> Result<kloudlite_workspaces::api::Judged, String> {
        use kloudlite_workspaces::api::{Judged, MemberState};
        Ok(if !self.is_team(team).await {
            Judged::TeamGone
        } else if user == "paula" {
            Judged::Member(MemberState::Paused)
        } else if self.members.contains(&(user, team)) {
            Judged::Member(MemberState::Active)
        } else {
            Judged::NotMember
        })
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
        if p == "ready" {
            let reason = if access == "readOnly" { "ReadOnly" } else { "Running" };
            b["status"]["conditions"] = json!([{"type": "Ready", "status": "True", "reason": reason, "message": "",
                                               "lastTransitionTime": "2026-09-13T10:00:00Z"}]);
        }
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
    let quota_reads = t.rec.calls().iter().filter(|c| *c == &format!("GET {API}/quotas/alice")).count();
    assert_eq!(quota_reads, 2, "the re-POST start is an allocation too: {:?}", t.rec.calls());
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
    assert_eq!(st, 410, "attach is chosen per space now");

    let ready = bench_obj("alice", "acme", "running", Some("ready"), "readOnly");
    let t = setup(vec![get(path.clone(), ready), region("r1")], Stub::new(&[], &[("acme", "r1")]));
    let (st, _) = t.call("POST", "/v1/bench/session?team=acme", &t.tok("alice"), None).await;
    assert_eq!(st, 201);

    let carol = bench_obj("carol", "acme", "running", Some("ready"), "full");
    let dave = bench_obj("dave", "dave", "running", Some("ready"), "full");
    let paula = bench_obj("paula", "acme", "running", Some("ready"), "full");
    let t = setup(
        vec![
            get(format!("{API}/benches"), list("Bench", vec![carol.clone(), dave.clone(), paula.clone()])),
            patch(bench_path("paula", "acme"), paula),
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
    assert!(t.rec.sent("PATCH", &bench_path("paula", "acme")).is_empty(), "pausing never rewrites a bench");
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

/// I2: a departed member's Ready bench still serving the old Full pod gets no token until the
/// reconciler has written a Ready reason for ReadOnly.
#[tokio::test]
async fn a_departed_member_gets_no_token_while_the_full_pod_still_serves() {
    let path = bench_path("alice", "acme");
    let full = bench_obj("alice", "acme", "running", Some("ready"), "full");
    let t = setup(vec![get(path.clone(), full.clone()), patch(path.clone(), full), region("r1")], Stub::new(&[], &[("acme", "r1")]));
    let (st, body) = t.call("POST", "/v1/bench/session?team=acme", &t.tok("alice"), None).await;
    assert_eq!((st, body), (StatusCode::ACCEPTED, json!({"state": "starting"})));
    assert_eq!(t.rec.sent("PATCH", &path)[0]["spec"]["access"], "readOnly");
}

/// I3: re-POSTing a stopped bench is a start, and a start at the cpu ceiling is refused.
#[tokio::test]
async fn re_posting_a_stopped_bench_at_the_cpu_limit_is_refused() {
    let path = bench_path("alice", "acme");
    let stopped = bench_obj("alice", "acme", "stopped", None, "full");
    let mut full = alloc("alice", vec![]);
    full[0] = get(
        format!("{API}/quotas/alice"),
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Quota", "metadata": {"name": "alice"},
               "spec": {"workspaces": 5, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 0, "memoryGb": 100}}),
    );
    let t = setup(with(vec![get(path.clone(), stopped.clone()), patch(path.clone(), stopped), region("r1")], full), Stub::new(&[("alice", "acme")], &[("acme", "r1")]));
    let (st, body) = t.call("POST", "/v1/bench", &t.tok("alice"), Some(json!({"team": "acme"}))).await;
    assert_eq!(st, 409, "{body}");
    assert!(t.rec.sent("PATCH", &path).is_empty());
}


fn tool_tok(t: &T, parent: &str) -> String {
    t.jwt.mint_bench_tool("alice", "acme", &bench_id("alice", "acme"), parent).unwrap().0
}

fn tool_setup(bench: Value) -> T {
    setup(
        with(vec![get(bench_path("alice", "acme"), bench), region("r1")], alloc("alice", vec![])),
        Stub::new(&[("alice", "acme")], &[("acme", "r1")]),
    )
}

#[tokio::test]
async fn a_bench_tool_token_lists_workspaces() {
    let t = tool_setup(bench_obj("alice", "acme", "running", Some("ready"), "full"));
    let (st, body) = t.call("GET", "/v1/workspaces?team=acme", &tool_tok(&t, LIVE_PARENT), None).await;
    assert_eq!(st, 200, "{body}");
    let (st, body) = t.call("GET", "/v1/workspaces", &tool_tok(&t, LIVE_PARENT), None).await;
    assert_eq!(st, 200, "{body}");
}

#[tokio::test]
async fn a_bench_tool_token_is_refused_off_its_routes() {
    let t = tool_setup(bench_obj("alice", "acme", "running", Some("ready"), "full"));
    let tok = tool_tok(&t, LIVE_PARENT);
    // `/v1/cli/*` and `/v1/keys*` live in crates/api, whose own test refuses this token.
    for (m, uri) in [
        ("POST", "/v1/bench/session?team=acme"),
        ("GET", "/v1/bench?team=acme"),
        ("POST", "/v1/workspaces/w1/ssh-session"),
        ("GET", "/v1/requests"),
        ("POST", "/v1/bench/tool-token?team=acme"),
        ("DELETE", "/v1/bench/tool-token?team=acme"),
    ] {
        let (st, _) = t.call(m, uri, &tok, None).await;
        assert_eq!(st, 401, "{m} {uri}: the gate, not the router");
    }
    for (m, uri) in [("GET", "/v1/keys"), ("GET", "/v1/cli/tokens")] {
        let (st, _) = t.call(m, uri, &tok, None).await;
        assert_eq!(st, 404, "{m} {uri}: not served by this router at all");
    }
    assert!(t.bench_writes().is_empty());
}

#[tokio::test]
async fn an_expired_bench_tool_token_is_refused_as_expired() {
    let t = tool_setup(bench_obj("alice", "acme", "running", Some("ready"), "full"));
    let (_, mut c) = t.jwt.mint_bench_tool("alice", "acme", &bench_id("alice", "acme"), LIVE_PARENT).unwrap();
    c.iat = 1;
    c.exp = 2;
    let tok = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &c,
        &jsonwebtoken::EncodingKey::from_secret(b"test-secret-at-least-32-bytes-long!!"),
    )
    .unwrap();
    let ((st, _), logs) = logged(t.call("GET", "/v1/workspaces", &tok, None)).await;
    assert_eq!(st, 401);
    assert!(logs.contains("bench.tool.refused") && logs.contains("expired") && logs.contains(&c.jti[..8]), "{logs}");
    assert!(!logs.contains(&c.jti), "{logs}");
}

#[tokio::test]
async fn a_bench_tool_token_dies_with_its_parent() {
    let t = tool_setup(bench_obj("alice", "acme", "running", Some("ready"), "full"));
    let (st, _) = t.call("GET", "/v1/workspaces", &tool_tok(&t, REVOKED_PARENT), None).await;
    assert_eq!(st, 401);
}

#[tokio::test]
async fn a_bench_tool_token_dies_when_the_bench_stops() {
    let t = tool_setup(bench_obj("alice", "acme", "stopped", Some("ready"), "full"));
    let (st, _) = t.call("GET", "/v1/workspaces", &tool_tok(&t, LIVE_PARENT), None).await;
    assert_eq!(st, 401);
}

#[tokio::test]
async fn a_bench_tool_token_is_refused_for_a_readonly_bench() {
    let t = tool_setup(bench_obj("alice", "acme", "running", Some("ready"), "readOnly"));
    let (st, _) = t.call("GET", "/v1/workspaces", &tool_tok(&t, LIVE_PARENT), None).await;
    assert_eq!(st, 401);
}

#[tokio::test]
async fn a_bench_tool_token_gets_403_on_another_team() {
    let t = setup(
        with(vec![get(bench_path("alice", "acme"), bench_obj("alice", "acme", "running", Some("ready"), "full"))], alloc("alice", vec![])),
        Stub::new(&[("alice", "acme"), ("alice", "t2")], &[("acme", "r1")]),
    );
    let (st, body) = t.call("GET", "/v1/volumes?owner=t2", &tool_tok(&t, LIVE_PARENT), None).await;
    assert_eq!(st, 403, "{body}");
    assert_eq!(body, Value::String("bench tools act only for alice and acme".into()));
}

fn space(owner: &str, team: &str) -> Value {
    serde_json::to_value(kloudlite_workspaces::crd::space_environment(owner, team, "env-1")).unwrap()
}

#[tokio::test]
async fn a_bench_tool_token_chooses_environments_only_in_its_own_spaces() {
    let spaces = list("SpaceEnvironment", vec![space("alice", "acme"), space("alice", "t2"), space("alice", "alice")]);
    let routes = || {
        vec![
            get(bench_path("alice", "acme"), bench_obj("alice", "acme", "running", Some("ready"), "full")),
            get(format!("{API}/spaceenvironments"), spaces.clone()),
        ]
    };
    let dir = || Stub::new(&[("alice", "acme"), ("alice", "t2")], &[]);
    let t = setup(routes(), dir());
    let tok = tool_tok(&t, LIVE_PARENT);
    for (m, uri, body) in [
        ("PUT", "/v1/me/environments/t2", Some(json!({"environment": "env-1"}))),
        ("PUT", "/v1/me/environments/T2", Some(json!({"environment": "env-1"}))),
        ("DELETE", "/v1/me/environments/t2", None),
    ] {
        let (st, body) = t.call(m, uri, &tok, body).await;
        assert_eq!(st, 403, "{m} {uri}: {body}");
    }
    let (st, body) = t.call("GET", "/v1/me/environments", &tok, None).await;
    assert_eq!(st, 200, "{body}");
    let teams: Vec<&str> = body.as_array().unwrap().iter().map(|x| x["team"].as_str().unwrap()).collect();
    assert_eq!(teams, ["acme", "alice"]);

    // An unscoped login is unchanged: every space listed, t2 not a scope refusal.
    let t = setup(routes(), dir());
    let (_, body) = t.call("GET", "/v1/me/environments", &t.tok("alice"), None).await;
    assert_eq!(body.as_array().unwrap().len(), 3, "{body}");
    let (st, _) = t.call("DELETE", "/v1/me/environments/t2", &t.tok("alice"), None).await;
    assert_ne!(st, 403);
}


// ── /v1/bench/tool-token ─────────────────────────────────────────────────

fn secret_path(owner: &str, team: &str) -> String {
    format!("/api/v1/namespaces/{}/secrets/bench-tool", kloudlite_workspaces::crd::ws_namespace(owner, team))
}

fn cli_tok(t: &T) -> String {
    t.jwt.mint_cli("alice@example.com", "Alice", Some("alice")).unwrap().0
}

fn secret_obj() -> Value {
    json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "bench-tool"}})
}

fn deleted() -> Value {
    json!({"kind": "Status", "apiVersion": "v1", "status": "Success", "code": 200})
}

fn route(method: &'static str, path: String, status: u16, body: Value) -> Route {
    Route { method, path, status, body }
}

/// Every log line written while `f` runs, as text.
async fn logged<F: std::future::Future<Output = R>, R>(f: F) -> (R, String) {
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buf = Buf::default();
    let w = buf.clone();
    let sub = tracing_subscriber::fmt().with_ansi(false).with_writer(move || w.clone()).finish();
    let _g = tracing::subscriber::set_default(sub);
    let r = f.await;
    let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    (r, text)
}

fn mint_setup(bench: Value, members: &[(&'static str, &'static str)], extra: Vec<Route>) -> T {
    setup(with(vec![get(bench_path("alice", "acme"), bench), region("r1")], extra), Stub::new(members, &[("acme", "r1")]))
}

#[tokio::test]
async fn tool_token_writes_the_secret_in_the_bench_namespace() {
    let sp = secret_path("alice", "acme");
    let t = mint_setup(bench_obj("alice", "acme", "running", Some("ready"), "full"), &[("alice", "acme")], vec![patch(sp.clone(), secret_obj())]);
    let ((st, body), logs) = logged(t.call("POST", "/v1/bench/tool-token?team=acme", &cli_tok(&t), None)).await;
    assert_eq!(st, 204);
    assert_eq!(body, Value::String(String::new()));
    let sent = t.rec.sent("PATCH", &sp);
    assert_eq!(sent.len(), 1);
    assert!(t.rec.requests().iter().any(|r| r.starts_with(&format!("PATCH {sp}?")) && r.contains("fieldManager=kloudlite-api")));
    let token = sent[0]["stringData"]["token"].as_str().unwrap();
    let claims = t.jwt.verify_bench_tool(token).unwrap();
    assert_eq!((claims.sub.as_str(), claims.team.as_str()), ("alice", "acme"));
    assert_eq!(sent[0]["metadata"]["annotations"]["kloudlite.io/exp"], claims.exp.to_string());
    assert!(logs.contains("bench.tool_token.written"), "{logs}");
    assert!(logs.contains(&claims.jti[..8]) && !logs.contains(&claims.jti) && !logs.contains(&claims.parent), "{logs}");
    assert!(!logs.contains(token), "{logs}");
}

#[tokio::test]
async fn tool_token_refuses_a_session_cookie() {
    let t = mint_setup(bench_obj("alice", "acme", "running", Some("ready"), "full"), &[("alice", "acme")], vec![]);
    let (st, body) = t.call("POST", "/v1/bench/tool-token?team=acme", &t.tok("alice"), None).await;
    assert_eq!(st, 403);
    assert_eq!(body["error"], "sign in on the Kloudlite desktop app");
    assert!(t.rec.sent("PATCH", &secret_path("alice", "acme")).is_empty());
}

#[tokio::test]
async fn tool_token_refuses_a_bench_tool_caller() {
    let t = mint_setup(bench_obj("alice", "acme", "running", Some("ready"), "full"), &[("alice", "acme")], vec![]);
    let (st, _) = t.call("POST", "/v1/bench/tool-token?team=acme", &tool_tok(&t, LIVE_PARENT), None).await;
    assert_eq!(st, 401);
    assert!(t.rec.sent("PATCH", &secret_path("alice", "acme")).is_empty());
}

#[tokio::test]
async fn tool_token_refuses_a_stopped_bench() {
    let t = mint_setup(bench_obj("alice", "acme", "stopped", Some("idle"), "full"), &[("alice", "acme")], vec![]);
    let (st, body) = t.call("POST", "/v1/bench/tool-token?team=acme", &cli_tok(&t), None).await;
    assert_eq!(st, 409);
    assert_eq!(body["error"], "bench is stopped; start it");
    assert!(t.rec.sent("PATCH", &secret_path("alice", "acme")).is_empty());
}

#[tokio::test]
async fn tool_token_refuses_a_departed_member() {
    let t = mint_setup(bench_obj("alice", "acme", "running", Some("ready"), "full"), &[], vec![]);
    let (st, _) = t.call("POST", "/v1/bench/tool-token?team=acme", &cli_tok(&t), None).await;
    assert_eq!(st, 403);
    assert!(t.rec.sent("PATCH", &secret_path("alice", "acme")).is_empty());
}

#[tokio::test]
async fn a_failed_secret_write_does_not_echo_the_body() {
    const ECHO: &str = "ECHOED-REQUEST-BODY eyJhbGciOiJIUzI1NiJ9";
    let fail = serde_json::to_value(kube::core::Status::failure(ECHO, "InternalError").with_code(500)).unwrap();
    let t = mint_setup(
        bench_obj("alice", "acme", "running", Some("ready"), "full"),
        &[("alice", "acme")],
        vec![route("PATCH", secret_path("alice", "acme"), 500, fail)],
    );
    let ((st, body), logs) = logged(t.call("POST", "/v1/bench/tool-token?team=acme", &cli_tok(&t), None)).await;
    assert_eq!(st, 503);
    let token = t.rec.sent("PATCH", &secret_path("alice", "acme"))[0]["stringData"]["token"].as_str().unwrap().to_string();
    for text in [body.to_string(), logs.clone()] {
        assert!(!text.contains("ECHOED") && !text.contains("eyJ") && !text.contains(&token), "{text}");
    }
    assert!(logs.contains("bench.tool_token.write.failed"), "{logs}");
}

#[tokio::test]
async fn stopping_a_bench_deletes_its_tool_secret() {
    let path = bench_path("alice", "acme");
    let b = bench_obj("alice", "acme", "running", Some("ready"), "full");
    let sp = secret_path("alice", "acme");
    let t = mint_setup(b.clone(), &[("alice", "acme")], vec![patch(path, b), route("DELETE", sp.clone(), 200, deleted())]);
    let (st, _) = t.call("POST", "/v1/bench/stop?team=acme", &t.tok("alice"), None).await;
    assert_eq!(st, 202);
    assert!(t.rec.calls().contains(&format!("DELETE {sp}")), "{:?}", t.rec.calls());
}

#[tokio::test]
async fn deleting_the_tool_token_deletes_the_secret_and_tolerates_404() {
    let sp = secret_path("alice", "acme");
    let b = bench_obj("alice", "acme", "running", Some("ready"), "full");
    for status in [200, 404] {
        let body = if status == 200 { deleted() } else { serde_json::to_value(kube::core::Status::failure("nf", "NotFound").with_code(404)).unwrap() };
        let t = mint_setup(b.clone(), &[("alice", "acme")], vec![route("DELETE", sp.clone(), status, body)]);
        let (st, _) = t.call("DELETE", "/v1/bench/tool-token?team=acme", &cli_tok(&t), None).await;
        assert_eq!(st, 204, "kube {status}");
        assert!(t.rec.calls().contains(&format!("DELETE {sp}")));
    }
}
