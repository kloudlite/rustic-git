//! The hidden per-owner builder environment (`bld-{slug}`, `spec.system: "builder"`): the api
//! writes it with an owner's first workspace, hides it from every user-facing environment route,
//! and starts/stops it only from behind `KLOUDLITE_BUILDER_SECRET`.
//!
//! In-process against the same mocked API server `api_packages.rs` uses.

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState, Directory, OwnerMaterial, TeamRole};
use kloudlite_workspaces::kube_test::{get, mock_client, patch, post, Recorder, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/kloudlite.io/v1alpha1";
const SECRET: &str = "builder-secret";

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

fn list_of(kind: &str, plural: &str, items: Vec<Value>) -> Route {
    get(
        format!("{API}/{plural}"),
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items}),
    )
}

fn region_obj() -> Route {
    get(
        format!("{API}/regions/centralindia"),
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Region",
               "metadata": {"name": "centralindia"}, "spec": {"name": "centralindia", "status": "active"}}),
    )
}

fn ws_obj(name: &str, owner: &str, team: &str) -> Value {
    json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "labels": {"kloudlite.io/owner": owner}},
        "spec": {
            "owner": owner, "team": team, "name": name, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": "running",
        },
        "status": {"phase": "ready", "nodeName": "node-a", "volumeRef": name},
    })
}

/// The builder as the api writes it, read back — what the hide/start/stop tests are about.
fn builder_obj(slug: &str, desired: &str, ready: bool) -> Value {
    json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
        "metadata": {"name": format!("bld-{slug}"), "resourceVersion": "7",
                     "labels": {"kloudlite.io/owner": slug}},
        "spec": {
            "owner": slug, "name": "builder", "region": "centralindia", "system": "builder",
            "services": [{"name": "buildkit", "image": "moby/buildkit:v0.18.2", "command": [],
                          "env": {}, "mounts": [], "ports": [1234]}],
            "storage": {"quotaGb": 50}, "desiredState": desired,
        },
        "status": {"phase": if ready { "running" } else { "creating" }, "nodeName": "node-a",
                   "volumeRef": format!("bld-{slug}"),
                   "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" },
                                   "reason": "Running", "message": "buildkit is up",
                                   "lastTransitionTime": "2026-09-09T00:00:00Z", "observedGeneration": 1}]},
    })
}

/// The quota gate's reads, with no `Quota` object anywhere so the compiled-in default applies,
/// plus the workspace POST and the builder's server-side apply.
fn base_routes() -> Vec<Route> {
    vec![
        region_obj(),
        empty("Workspace", "workspaces"),
        empty("Environment", "environments"),
        empty("Snapshot", "snapshots"),
        empty("Volume", "volumes"),
        kloudlite_workspaces::kube_test::not_found(format!("{API}/quotas/karthik")),
        kloudlite_workspaces::kube_test::not_found(format!("{API}/quotas/default-user")),
        kloudlite_workspaces::kube_test::not_found(format!("{API}/quotas/acme")),
        kloudlite_workspaces::kube_test::not_found(format!("{API}/quotas/default-team")),
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik", "")),
        patch(format!("{API}/environments/bld-karthik"), builder_obj("karthik", "stopped", false)),
        patch(format!("{API}/environments/bld-acme"), builder_obj("acme", "stopped", false)),
    ]
}

struct StubTeams;

#[async_trait::async_trait]
impl Directory for StubTeams {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        if user == "karthik" { vec!["acme".into()] } else { vec![] }
    }
    async fn is_live(&self, _jti: &str) -> bool {
        false
    }
    async fn for_owner(&self, _owner: &str) -> Option<OwnerMaterial> {
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
}

async fn server(routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(jwt.clone())
        .with_kube(client)
        .with_directory(Arc::new(StubTeams))
        .with_builder_secret(Some(SECRET.to_string()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn token(jwt: &Jwt) -> String {
    jwt.mint("karthik@example.com", "Test User", Some("karthik")).unwrap()
}

async fn create_ws(s: &Server, team: Option<&str>) -> reqwest::Response {
    let mut body = json!({"name": "web", "region": "centralindia", "quota_gb": 20});
    if let Some(t) = team {
        body["team"] = json!(t);
    }
    reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt))
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn applied(s: &Server, slug: &str) -> Vec<Value> {
    s.rec.sent("PATCH", &format!("{API}/environments/bld-{slug}"))
}

#[tokio::test]
async fn creating_a_workspace_writes_the_owners_builder() {
    let s = server(base_routes()).await;
    assert_eq!(create_ws(&s, None).await.status(), 202);
    let w = applied(&s, "karthik");
    assert_eq!(w.len(), 1, "one server-side apply: {w:?}");
    let spec = &w[0]["spec"];
    assert_eq!(w[0]["metadata"]["name"], "bld-karthik");
    assert_eq!(spec["owner"], "karthik");
    assert_eq!(spec["system"], "builder");
    assert_eq!(spec["region"], "centralindia");
    assert_eq!(spec["desiredState"], "stopped");
    assert_eq!(spec["storage"]["quotaGb"], 50);
    let svcs = spec["services"].as_array().expect("one service");
    assert_eq!(svcs.len(), 1);
    assert_eq!(svcs[0]["name"], "buildkit");
    assert_eq!(svcs[0]["image"], "moby/buildkit:v0.18.2");
    assert_eq!(
        svcs[0]["command"],
        json!(["buildkitd", "--addr", "tcp://0.0.0.0:1234", "--oci-worker-snapshotter=native", "--root", "/cache"])
    );
    assert_eq!(svcs[0]["ports"], json!([1234]));
    assert_eq!(svcs[0]["mounts"], json!([{"folder": "cache", "path": "/cache"}]));
    assert!(svcs[0]["resources"].is_object(), "the builder gets real headroom: {}", svcs[0]);
}

#[tokio::test]
async fn a_second_workspace_applies_the_same_builder() {
    let s = server(base_routes()).await;
    assert_eq!(create_ws(&s, None).await.status(), 202);
    assert_eq!(create_ws(&s, None).await.status(), 202);
    let w = applied(&s, "karthik");
    assert_eq!(w.len(), 2);
    assert_eq!(w[0], w[1], "server-side apply is idempotent, byte for byte");
}

#[tokio::test]
async fn a_team_workspace_writes_the_teams_builder() {
    let s = server(base_routes()).await;
    assert_eq!(create_ws(&s, Some("acme")).await.status(), 202);
    let w = applied(&s, "acme");
    assert_eq!(w.len(), 1, "the team's builder, not the person's: {w:?}");
    assert_eq!(w[0]["spec"]["owner"], "acme");
    assert!(applied(&s, "karthik").is_empty(), "a team workspace writes no personal builder");
}

#[tokio::test]
async fn a_failed_builder_write_never_fails_the_workspace() {
    // No PATCH route for the builder at all: the mock answers 404 and the create still succeeds.
    let routes: Vec<Route> = base_routes()
        .into_iter()
        .filter(|r| !r.path.starts_with(&format!("{API}/environments/bld-")))
        .collect();
    let s = server(routes).await;
    assert_eq!(create_ws(&s, None).await.status(), 202);
}

/// Every user-facing environment route, on a builder id. The one guard lives in the lookup they
/// all share, so a route added later cannot forget it — this is the proof it holds today.
#[tokio::test]
async fn the_builder_is_invisible_to_every_environment_route() {
    let routes = vec![
        get(format!("{API}/environments/bld-karthik"), builder_obj("karthik", "stopped", false)),
        empty("Snapshot", "snapshots"),
        empty("Volume", "volumes"),
        empty("Workspace", "workspaces"),
        // So `attach_ws` gets past its workspace lookup and refuses on the ENVIRONMENT.
        get(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik", "")),
        patch(format!("{API}/environments/bld-karthik"), builder_obj("karthik", "running", false)),
    ];
    let s = server(routes).await;
    let c = reqwest::Client::new();
    let base = &s.base;
    let calls: Vec<(&str, String, Value)> = vec![
        ("GET", format!("{base}/v1/environments/bld-karthik"), Value::Null),
        ("DELETE", format!("{base}/v1/environments/bld-karthik"), Value::Null),
        ("POST", format!("{base}/v1/environments/bld-karthik/start"), Value::Null),
        ("POST", format!("{base}/v1/environments/bld-karthik/stop"), Value::Null),
        ("POST", format!("{base}/v1/environments/bld-karthik/clone"), json!({"name": "copy"})),
        ("POST", format!("{base}/v1/environments/bld-karthik/push"), json!({})),
        ("POST", format!("{base}/v1/environments/bld-karthik/restore-in-place"), json!({"snapshot_id": "s1"})),
        ("POST", format!("{base}/v1/environments/bld-karthik/intercepts"), json!({"service": "buildkit", "workspace": "ws-1", "ports": []})),
        ("DELETE", format!("{base}/v1/environments/bld-karthik/intercepts/buildkit"), Value::Null),
        // Not a route at all, and a 404 either way: the gate's own surface is `/v1/internal`.
        ("GET", format!("{base}/v1/environments/bld-karthik/snapshots"), Value::Null),
        // Attaching a workspace to the builder resolves the environment through the same lookup.
        ("POST", format!("{base}/v1/workspaces/ws-1/attach"), json!({"environment": "bld-karthik"})),
    ];
    for (method, url, body) in calls {
        let mut req = c.request(method.parse().unwrap(), &url).bearer_auth(token(&s.jwt));
        if !body.is_null() {
            req = req.json(&body);
        }
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 404, "{method} {url} must 404 on a builder");
    }
}

#[tokio::test]
async fn list_env_omits_the_builder() {
    let routes = vec![
        list_of(
            "Environment",
            "environments",
            vec![
                builder_obj("karthik", "running", true),
                json!({
                    "apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
                    "metadata": {"name": "env-1", "labels": {"kloudlite.io/owner": "karthik"}},
                    "spec": {"owner": "karthik", "name": "app", "region": "centralindia",
                             "services": [], "storage": {"quotaGb": 20}, "desiredState": "running"},
                }),
            ],
        ),
        empty("Snapshot", "snapshots"),
    ];
    let s = server(routes).await;
    let r = reqwest::Client::new()
        .get(format!("{}/v1/environments", s.base))
        .bearer_auth(token(&s.jwt))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let ids: Vec<&str> = body.as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["env-1"], "the builder is nobody's environment");
}

async fn internal(s: &Server, method: &str, path: &str, secret: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new().request(method.parse().unwrap(), format!("{}{path}", s.base));
    if let Some(t) = secret {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn the_internal_routes_refuse_without_the_secret() {
    let s = server(vec![get(format!("{API}/environments/bld-karthik"), builder_obj("karthik", "stopped", false))]).await;
    for (m, p) in [
        ("GET", "/v1/internal/builders"),
        ("GET", "/v1/internal/builders/karthik"),
        ("POST", "/v1/internal/builders/karthik/start"),
        ("POST", "/v1/internal/builders/karthik/stop"),
    ] {
        assert_eq!(internal(&s, m, p, None).await.status(), 401, "{m} {p} with no secret");
        // A user's own session token is not the gate's secret.
        assert_eq!(internal(&s, m, p, Some(&token(&s.jwt))).await.status(), 401, "{m} {p} with a user token");
    }
}

#[tokio::test]
async fn start_and_stop_write_the_desired_state() {
    let s = server(vec![patch(format!("{API}/environments/bld-karthik"), builder_obj("karthik", "running", false))]).await;
    for (path, want) in [("start", "running"), ("stop", "stopped")] {
        let r = internal(&s, "POST", &format!("/v1/internal/builders/karthik/{path}"), Some(SECRET)).await;
        assert_eq!(r.status(), 202, "{path}");
        let sent = s.rec.sent("PATCH", &format!("{API}/environments/bld-karthik"));
        assert_eq!(sent.last().unwrap()["spec"]["desiredState"], want);
    }
}

#[tokio::test]
async fn get_reports_ready_from_the_condition() {
    for ready in [false, true] {
        let s = server(vec![get(format!("{API}/environments/bld-karthik"), builder_obj("karthik", "running", ready))]).await;
        let r = internal(&s, "GET", "/v1/internal/builders/karthik", Some(SECRET)).await;
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["id"], "bld-karthik");
        assert_eq!(body["ready"], ready);
        assert_eq!(body["state"], if ready { "running" } else { "creating" });
        assert_eq!(body["conditions"][0]["type"], "Ready");
    }
}

#[tokio::test]
async fn get_is_404_for_an_owner_with_no_builder() {
    let s = server(vec![]).await;
    assert_eq!(internal(&s, "GET", "/v1/internal/builders/nobody", Some(SECRET)).await.status(), 404);
}

#[tokio::test]
async fn the_list_names_every_running_builder() {
    let s = server(vec![list_of(
        "Environment",
        "environments",
        vec![
            builder_obj("acme", "running", true),
            builder_obj("alice", "stopped", false),
            builder_obj("bob", "running", false),
            json!({
                "apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
                "metadata": {"name": "env-1", "labels": {"kloudlite.io/owner": "karthik"}},
                "spec": {"owner": "karthik", "name": "app", "region": "centralindia",
                         "services": [], "storage": {"quotaGb": 20}, "desiredState": "running"},
            }),
        ],
    )])
    .await;
    let r = internal(&s, "GET", "/v1/internal/builders", Some(SECRET)).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["running"], json!(["acme", "bob"]), "running builders only, and no real environment");
}

#[tokio::test]
async fn the_builder_costs_disk_and_capacity_but_is_not_an_environment() {
    let real_env = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
        "metadata": {"name": "env-1", "labels": {"kloudlite.io/owner": "karthik"}},
        "spec": {"owner": "karthik", "name": "app", "region": "centralindia",
                 "services": [{"name": "db", "image": "mongo", "command": [], "env": {}, "mounts": [], "ports": []}],
                 "storage": {"quotaGb": 20}, "desiredState": "running"},
    });
    let vol = |name: &str, gb: u64| {
        json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
               "metadata": {"name": name, "labels": {"kloudlite.io/owner": "karthik"}},
               "spec": {"owner": "karthik", "team": "", "nodeName": "node-a", "region": "centralindia",
                        "quotaGb": gb, "replicas": 1}})
    };
    let running = |desired: &str| {
        let mut b = builder_obj("karthik", desired, true);
        b["spec"]["services"][0]["resources"] =
            serde_json::to_value(kloudlite_workspaces::crd::PodResources::default()).unwrap();
        b
    };
    for (desired, cpu, mem) in [("running", 6, 12), ("stopped", 2, 4)] {
        let s = server(vec![
            empty("Workspace", "workspaces"),
            list_of("Environment", "environments", vec![real_env.clone(), running(desired)]),
            list_of("Volume", "volumes", vec![vol("env-1", 20), vol("bld-karthik", 50)]),
            empty("Snapshot", "snapshots"),
            kloudlite_workspaces::kube_test::not_found(format!("{API}/quotas/karthik")),
            kloudlite_workspaces::kube_test::not_found(format!("{API}/quotas/default-user")),
        ])
        .await;
        let r = reqwest::Client::new()
            .get(format!("{}/v1/quota", s.base))
            .bearer_auth(token(&s.jwt))
            .send()
            .await
            .unwrap();
        let st = r.status();
        let body: Value = serde_json::from_str(&r.text().await.unwrap()).unwrap_or(Value::Null);
        assert_eq!(st, 200, "{body}");
        let used = &body["used"];
        assert_eq!(used["environments"], 1, "the builder is nobody's environment: {used}");
        assert_eq!(used["diskGb"], 70, "its cache is still the owner's disk: {used}");
        assert_eq!(used["cpu"], cpu, "desiredState {desired}: {used}");
        assert_eq!(used["memoryGb"], mem, "desiredState {desired}: {used}");
    }
}
