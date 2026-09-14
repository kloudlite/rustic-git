//! `/v1/me/environments`: a person chooses one environment per space. In-process against the mocked
//! API server; what matters is what was (and was not) written.

mod common;
use common::token;

use kloudlite_core::jwt::Jwt;
use kloudlite_workspaces::api::{router, ApiState, Directory};
use kloudlite_workspaces::crd;
use kloudlite_workspaces::kube_test::{get, mock_client, Recorder, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/kloudlite.io/v1alpha1";

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
}

/// `karthik` is a member of `acme`, and `acme` is the only team that exists. `fail` makes the
/// error-propagating membership read fail, as an unreadable user collection would.
#[derive(Default)]
struct Stub {
    fail: bool,
}

#[async_trait::async_trait]
impl Directory for Stub {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        if user == "karthik" { vec!["acme".into()] } else { vec![] }
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
    async fn team_role(&self, _user: &str, _team: &str) -> Option<kloudlite_workspaces::api::TeamRole> {
        None
    }
    async fn is_team(&self, slug: &str) -> bool {
        slug == "acme"
    }
    async fn member_teams(&self, user: &str) -> Result<Vec<String>, String> {
        if self.fail { Err("users unreadable".into()) } else { Ok(self.teams_for(user).await) }
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
    let state = ApiState::new(jwt.clone()).with_kube(client).with_directory(Arc::new(Stub::default()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec }
}

fn env(id: &str, owner: &str, system: Option<&str>) -> Value {
    json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Environment",
        "metadata": {"name": id, "labels": {"kloudlite.io/owner": owner}},
        "spec": {"owner": owner, "name": id, "region": "centralindia", "services": [], "desiredState": "running", "system": system},
    })
}

fn space_path(owner: &str, team: &str) -> String {
    format!("{API}/spaceenvironments/{}", crd::space_name(owner, team))
}

fn applied(owner: &str, team: &str) -> Route {
    kloudlite_workspaces::kube_test::patch(space_path(owner, team), serde_json::to_value(crd::space_environment(owner, team, "env-1")).unwrap())
}

async fn put(s: &Server, team: &str, body: Value) -> (reqwest::StatusCode, String) {
    let r = reqwest::Client::new()
        .put(format!("{}/v1/me/environments/{team}", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status(), r.text().await.unwrap())
}

fn writes(s: &Server) -> Vec<String> {
    s.rec.calls().into_iter().filter(|c| !c.starts_with("GET")).collect()
}

#[tokio::test]
async fn choosing_writes_the_callers_own_space_and_nothing_else() {
    let s = server(vec![get(format!("{API}/environments/env-1"), env("env-1", "acme", None)), applied("karthik", "acme")]).await;
    let (st, body) = put(&s, "Acme", json!({"environment": "env-1"})).await;
    assert_eq!(st, 200, "{body}");
    let sent = s.rec.sent("PATCH", &space_path("karthik", "acme"));
    assert_eq!(sent.len(), 1, "{:?}", s.rec.calls());
    assert_eq!(sent[0]["spec"], json!({"owner": "karthik", "team": "acme", "environment": "env-1"}));
    assert_eq!(sent[0]["metadata"]["labels"]["kloudlite.io/environment"], "env-1");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), json!({"team": "acme", "environment": "env-1", "region": "centralindia"}));
}

#[tokio::test]
async fn a_personal_space_may_choose_only_the_persons_own_environment() {
    let s = server(vec![
        get(format!("{API}/environments/env-1"), env("env-1", "karthik", None)),
        get(format!("{API}/environments/env-team"), env("env-team", "acme", None)),
        applied("karthik", "karthik"),
    ])
    .await;
    assert_eq!(put(&s, "karthik", json!({"environment": "env-1"})).await.0, 200);
    assert_eq!(s.rec.sent("PATCH", &space_path("karthik", "")).len(), 1, "the personal space is ws-karthik");
    let (st, body) = put(&s, "karthik", json!({"environment": "env-team"})).await;
    assert_eq!(st, 409, "{body}");
}

/// The authorization matrix: every refusal writes nothing.
#[tokio::test]
async fn every_refusal_writes_nothing() {
    let s = server(vec![
        get(format!("{API}/environments/env-other"), env("env-other", "someone", None)),
        get(format!("{API}/environments/bld-acme"), env("bld-acme", "acme", Some(crd::BUILDER_SYSTEM))),
    ])
    .await;
    let cases = [
        ("acme", json!({"environment": "env-other", "owner": "bob"}), 400, "a body naming an owner"),
        ("other", json!({"environment": "env-other"}), 404, "a team the caller is not in"),
        ("acme", json!({"environment": "env-other"}), 409, "another owner's environment"),
        ("acme", json!({"environment": "bld-acme"}), 404, "the hidden builder"),
        ("acme", json!({"environment": "env-gone"}), 404, "no such environment"),
        ("acme", json!({"environment": "env-1/../x"}), 422, "not a label value"),
    ];
    for (team, body, want, why) in cases {
        let (st, text) = put(&s, team, body).await;
        assert_eq!(st.as_u16(), want, "{why}: {text}");
    }
    assert!(writes(&s).is_empty(), "{:?}", s.rec.calls());
}

#[tokio::test]
async fn clearing_is_idempotent_and_listing_is_the_callers_own() {
    let mine = serde_json::to_value(crd::space_environment("karthik", "acme", "env-1")).unwrap();
    let mut theirs = serde_json::to_value(crd::space_environment("bob", "acme", "env-1")).unwrap();
    theirs["metadata"]["labels"]["kloudlite.io/owner"] = json!("karthik");
    let s = server(vec![
        get(format!("{API}/spaceenvironments"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "SpaceEnvironmentList", "metadata": {}, "items": [mine, theirs]})),
        get(format!("{API}/environments/env-1"), env("env-1", "acme", None)),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");
    let (st, body) = common::get_json(&s.base, &tok, "/v1/me/environments").await;
    assert_eq!(st, 200);
    assert_eq!(body, json!([{"team": "acme", "environment": "env-1", "region": "centralindia"}]), "a mislabelled row is not the caller's");
    // Nothing to delete (the mock 404s) is still the state asked for.
    assert_eq!(common::delete(&s.base, &tok, "/v1/me/environments/acme").await, 204);
    assert!(s.rec.calls().contains(&format!("DELETE {}", space_path("karthik", "acme"))), "{:?}", s.rec.calls());
}

#[tokio::test]
async fn the_retired_attach_routes_answer_gone() {
    let s = server(vec![]).await;
    let tok = token(&s.jwt, "karthik");
    for path in ["/v1/workspaces/ws-1/attach", "/v1/workspaces/ws-1/detach", "/v1/bench/attach", "/v1/bench/detach"] {
        let r = reqwest::Client::new().post(format!("{}{path}", s.base)).bearer_auth(&tok).json(&json!({"environment": "env-1"})).send().await.unwrap();
        assert_eq!(r.status(), 410, "{path}");
        assert!(r.text().await.unwrap().contains("/v1/me/environments/"), "{path}");
    }
    assert!(writes(&s).is_empty());
}

/// Deleting an environment deletes every space choice naming it — selected by the label, re-checked
/// against spec.
#[tokio::test]
async fn deleting_an_environment_deletes_the_choices_naming_it() {
    let e = env("env-1", "karthik", None);
    let named = serde_json::to_value(crd::space_environment("karthik", "karthik", "env-1")).unwrap();
    let mut stale_label = serde_json::to_value(crd::space_environment("karthik", "acme", "env-2")).unwrap();
    stale_label["metadata"]["labels"]["kloudlite.io/environment"] = json!("env-1");
    let s = server(vec![
        get(format!("{API}/environments/env-1"), e.clone()),
        Route { method: "DELETE", path: format!("{API}/environments/env-1"), status: 200, body: e },
        get(format!("{API}/spaceenvironments"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "SpaceEnvironmentList", "metadata": {}, "items": [named.clone(), stale_label]})),
        Route { method: "DELETE", path: space_path("karthik", "karthik"), status: 200, body: named },
        get(format!("{API}/snapshots"), json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []})),
    ])
    .await;
    let st = common::delete(&s.base, &token(&s.jwt, "karthik"), "/v1/environments/env-1").await;
    assert!(st.is_success(), "{st}");
    let calls = s.rec.calls();
    assert!(calls.iter().any(|c| c.starts_with(&format!("GET {API}/spaceenvironments")) && c.contains("environment")), "{calls:?}");
    assert!(calls.contains(&format!("DELETE {}", space_path("karthik", "karthik"))), "{calls:?}");
    assert!(!calls.contains(&format!("DELETE {}", space_path("karthik", "acme"))), "the label is a view: {calls:?}");
}


// ── the keys-beat halves ─────────────────────────────────────────────────

const DS: &str = "/apis/apps/v1/namespaces/kube-system/daemonsets/kloudlite-agent";

fn state(routes: Vec<Route>, fail: bool) -> (ApiState, Recorder) {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    (ApiState::new(jwt).with_kube(client).with_directory(Arc::new(Stub { fail })), rec)
}

fn list(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "kloudlite.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn legacy_ws(name: &str, env: &str, at: &str, settled: bool) -> Value {
    let mut w = json!({
        "apiVersion": "kloudlite.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "managedFields": [{"manager": "x", "operation": "Update", "time": at}]},
        "spec": {"owner": "karthik", "team": "", "name": name, "region": "centralindia", "image": "i", "desiredState": "running", "attachedEnvironment": env},
    });
    if settled {
        w["metadata"]["annotations"] = json!({crd::SPACE_MIGRATED_ANNOTATION: "true"});
    }
    w
}

fn agents(rolled: bool) -> Route {
    get(DS, json!({
        "apiVersion": "apps/v1", "kind": "DaemonSet", "metadata": {"name": "kloudlite-agent", "namespace": "kube-system"},
        "spec": {"selector": {}, "template": {"metadata": {"annotations": {"kloudlite.io/space-env": "1"}}}},
        "status": {"currentNumberScheduled": 3, "desiredNumberScheduled": 3, "numberMisscheduled": 0, "numberReady": 3,
                   "updatedNumberScheduled": if rolled { 3 } else { 2 }, "numberAvailable": 3},
    }))
}

fn beat_routes(spaces: Vec<Value>, workspaces: Vec<Value>, rolled: bool, create_status: u16) -> Vec<Route> {
    let mut r = vec![
        get(format!("{API}/spaceenvironments"), list("SpaceEnvironment", spaces)),
        get(format!("{API}/workspaces"), list("Workspace", workspaces.clone())),
        get(format!("{API}/environments/env-1"), env("env-1", "karthik", None)),
        get(format!("{API}/environments/env-2"), env("env-2", "karthik", None)),
        Route { method: "POST", path: format!("{API}/spaceenvironments"), status: create_status, body: if create_status == 409 {
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "AlreadyExists", "code": 409, "message": "exists"})
        } else {
            serde_json::to_value(crd::space_environment("karthik", "karthik", "env-1")).unwrap()
        } },
        agents(rolled),
    ];
    for w in workspaces {
        let name = w["metadata"]["name"].as_str().unwrap().to_string();
        r.push(kloudlite_workspaces::kube_test::patch(format!("{API}/workspaces/{name}"), w));
    }
    r
}

fn ws_patch(rec: &Recorder, name: &str) -> Vec<Value> {
    rec.sent("PATCH", &format!("{API}/workspaces/{name}"))
}

#[tokio::test]
async fn migration_writes_the_choice_and_clears_the_field_once_every_agent_reads_choices() {
    let (s, rec) = state(beat_routes(vec![], vec![legacy_ws("ws-1", "env-1", "2026-09-14T10:00:00Z", false)], true, 201), false);
    kloudlite_workspaces::api::spaces::migrate(&s).await;
    let posted = rec.sent("POST", &format!("{API}/spaceenvironments"));
    assert_eq!(posted.len(), 1, "{:?}", rec.calls());
    assert_eq!(posted[0]["spec"]["environment"], "env-1");
    let p = ws_patch(&rec, "ws-1");
    assert_eq!(p.len(), 1);
    assert!(p[0]["spec"]["attachedEnvironment"].is_null() && p[0]["spec"].is_object(), "{}", p[0]);
    assert_eq!(p[0]["metadata"]["annotations"][crd::SPACE_MIGRATED_ANNOTATION], "true");
}

#[tokio::test]
async fn a_clear_is_deferred_while_an_agent_has_not_rolled_but_the_object_is_stamped() {
    let (s, rec) = state(beat_routes(vec![], vec![legacy_ws("ws-1", "env-1", "2026-09-14T10:00:00Z", false)], false, 201), false);
    kloudlite_workspaces::api::spaces::migrate(&s).await;
    assert_eq!(rec.sent("POST", &format!("{API}/spaceenvironments")).len(), 1);
    let p = ws_patch(&rec, "ws-1");
    assert_eq!(p.len(), 1);
    assert!(p[0].get("spec").is_none(), "the field stays for an old agent: {}", p[0]);
    assert_eq!(p[0]["metadata"]["annotations"][crd::SPACE_MIGRATED_ANNOTATION], "true");
}

#[tokio::test]
async fn in_a_conflict_the_newest_attach_is_the_choice() {
    let (s, rec) = state(
        beat_routes(vec![], vec![legacy_ws("ws-old", "env-1", "2026-09-14T09:00:00Z", false), legacy_ws("ws-new", "env-2", "2026-09-14T10:00:00Z", false)], true, 201),
        false,
    );
    kloudlite_workspaces::api::spaces::migrate(&s).await;
    let posted = rec.sent("POST", &format!("{API}/spaceenvironments"));
    assert_eq!(posted.len(), 1, "one space, one choice");
    assert_eq!(posted[0]["spec"]["environment"], "env-2");
    assert_eq!(ws_patch(&rec, "ws-old").len(), 1, "the loser's field is cleared too");
}

/// A settled object whose clear failed is retried on the next beat — and never turned back into a
/// choice, even though its space has none now (the person cleared it).
#[tokio::test]
async fn a_failed_clear_is_retried_without_resurrecting_a_cleared_choice() {
    let (s, rec) = state(beat_routes(vec![], vec![legacy_ws("ws-1", "env-1", "2026-09-14T10:00:00Z", true)], true, 201), false);
    kloudlite_workspaces::api::spaces::migrate(&s).await;
    assert!(rec.sent("POST", &format!("{API}/spaceenvironments")).is_empty(), "{:?}", rec.calls());
    let p = ws_patch(&rec, "ws-1");
    assert_eq!(p.len(), 1);
    assert!(p[0]["spec"]["attachedEnvironment"].is_null() && p[0]["spec"].is_object());
}

/// A choice created between the list and the write wins: the migration creates, never applies.
#[tokio::test]
async fn migration_never_overwrites_a_choice_made_meanwhile() {
    let (s, rec) = state(beat_routes(vec![], vec![legacy_ws("ws-1", "env-1", "2026-09-14T10:00:00Z", false)], true, 409), false);
    kloudlite_workspaces::api::spaces::migrate(&s).await;
    assert!(!rec.calls().iter().any(|c| (c.starts_with("PATCH") || c.starts_with("PUT")) && c.contains("spaceenvironments")), "{:?}", rec.calls());
    assert_eq!(ws_patch(&rec, "ws-1").len(), 1, "the space is settled, so the field goes");
}

#[tokio::test]
async fn a_departed_members_choice_goes_and_any_directory_error_prunes_nothing() {
    let choice = |owner: &str| serde_json::to_value(crd::space_environment(owner, "acme", "env-1")).unwrap();
    let routes = || vec![
        get(format!("{API}/spaceenvironments"), list("SpaceEnvironment", vec![choice("karthik"), choice("bob"), serde_json::to_value(crd::space_environment("bob", "bob", "env-9")).unwrap()])),
        Route { method: "DELETE", path: space_path("bob", "acme"), status: 200, body: choice("bob") },
    ];
    let (s, rec) = state(routes(), false);
    kloudlite_workspaces::api::spaces::prune_departed(&s).await;
    let deletes: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(deletes, vec![format!("DELETE {}", space_path("bob", "acme"))], "only bob left acme; personal spaces are never pruned");

    let (s, rec) = state(routes(), true);
    kloudlite_workspaces::api::spaces::prune_departed(&s).await;
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
}
