//! `GET /admin/requests` — one queue over `Request` and the legacy `QuotaRequest` CRD, so a
//! console never has to know whether the migration to the generic CRD has run.

use kloudlite_git_core::jwt::Jwt;
use kloudlite_git_workspaces::api::{admin::router, ApiState, Directory, GrantAccess, TeamRole};
use kloudlite_git_workspaces::kube_test::{
    conflict as route_conflict, get as route_get, mock_client, not_found, patch as route_patch,
    post as route_post, Recorder, Route,
};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/kloudlite-git.io/v1alpha1";

/// `grant` is what `grant_access` answers and `granted` is what it was asked — the access arm's
/// whole contract is "which outcome maps to which status, and who was the grant for", and neither
/// half is observable without both.
///
/// `handles` makes the stub key the way the real directory does: it is handed a HANDLE and its
/// memberships live under an EMAIL, so a caller that hands over the wrong identity gets
/// `NoSuchUser` here exactly as it would in production.
#[derive(Default)]
struct StubMembership {
    grant: Option<GrantAccess>,
    handles: std::collections::HashMap<String, String>,
    /// `acme`'s members, keyed the way the directory keys them: by email, never by handle.
    team: Vec<(String, TeamRole)>,
    /// The directory cannot be read at all — the arm that must NOT read as "no such user".
    unreadable: bool,
    granted: std::sync::Mutex<Vec<(String, String, TeamRole)>>,
}

impl StubMembership {
    fn answering(grant: GrantAccess) -> Self {
        Self {
            grant: Some(grant),
            handles: [("meera".to_string(), "meera@example.com".to_string())].into(),
            team: vec![("meera@example.com".to_string(), TeamRole::Member)],
            unreadable: false,
            granted: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl Directory for StubMembership {
    // Both of these take a HANDLE and answer from memberships held under an EMAIL, exactly as the
    // directory does — team scoping and the team-admin check compare `members.user`, so an
    // unresolved handle is no teams and no role rather than a comparison against an address.
    async fn teams_for(&self, user: &str) -> Vec<String> {
        match self.handles.get(user) {
            Some(email) if self.team.iter().any(|(m, _)| m == email) => vec!["acme".into()],
            _ => vec![],
        }
    }

    async fn team_role(&self, user: &str, team: &str) -> Option<TeamRole> {
        let email = self.handles.get(user)?;
        self.team.iter().find(|(m, _)| m == email && team == "acme").map(|(_, r)| *r)
    }

    async fn is_team(&self, _slug: &str) -> bool {
        false
    }
    async fn ensure_user(&self, _e: &str, _n: &str, _u: &str) -> Result<(), String> {
        Err("no directory".into())
    }

    async fn is_live(&self, _jti: &str) -> bool {
        false
    }

    async fn for_owner(&self, _owner: &str) -> Option<kloudlite_git_workspaces::api::OwnerMaterial> {
        None
    }

    async fn grant_access(&self, team: &str, user: &str, role: TeamRole) -> GrantAccess {
        if self.unreadable {
            return GrantAccess::Refused("the directory could not be read".into());
        }
        let Some(email) = self.handles.get(user) else { return GrantAccess::NoSuchUser };
        self.granted.lock().unwrap().push((team.into(), email.clone(), role));
        // `GrantAccess` is deliberately not `Clone` (the refusal string is the directory's own
        // words, handed over once), so the canned answer is rebuilt rather than copied.
        match &self.grant {
            Some(GrantAccess::Done) => GrantAccess::Done,
            Some(GrantAccess::NoSuchUser) => GrantAccess::NoSuchUser,
            Some(GrantAccess::NoSuchTeam) => GrantAccess::NoSuchTeam,
            Some(GrantAccess::Refused(why)) => GrantAccess::Refused(why.clone()),
            _ => GrantAccess::Unsupported,
        }
    }}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    rec: Recorder,
    dir: Arc<StubMembership>,
}

async fn admin_server(routes: Vec<Route>) -> Server {
    admin_server_with(routes, StubMembership::default()).await
}

async fn admin_server_with(routes: Vec<Route>, dir: StubMembership) -> Server {
    let dir = Arc::new(dir);
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let mut state = ApiState::new(jwt.clone());
    state = state.with_directory(dir.clone());
    let (client, rec) = mock_client(routes);
    state = state.with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(Arc::new(state));
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server { base: format!("http://{addr}"), jwt, rec, dir }
}

fn admin_token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

async fn get(url: &str, token: &str) -> reqwest::Response {
    reqwest::Client::new().get(url).bearer_auth(token).send().await.unwrap()
}

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

/// The queue is one list over two CRDs while the legacy objects still exist: a console must not
/// have to know that a migration is half-done.
#[tokio::test]
async fn the_queue_unions_legacy_quota_requests() {
    let s = admin_server(vec![
        route_get(
            format!("{API}/requests"),
            list_of(
                "Request",
                vec![json!({
                    "metadata": {"name": "req-1", "creationTimestamp": "2026-09-04T10:00:00Z"},
                    "spec": {"owner": "acme", "kind": "access", "requestedBy": "meera", "reason": "r",
                             "access": {"team": "acme", "role": "admin"}},
                    "status": {"state": "pending"}
                })],
            ),
        ),
        route_get(
            format!("{API}/quotarequests"),
            list_of(
                "QuotaRequest",
                vec![json!({
                    "metadata": {"name": "qr-9", "creationTimestamp": "2026-09-03T10:00:00Z"},
                    "spec": {"owner": "zoe", "requested": {"cpu": 12}, "reason": "old"},
                    "status": {"state": "pending"}
                })],
            ),
        ),
    ])
    .await;
    let r = get(&format!("{}/admin/requests", s.base), &admin_token(&s.jwt)).await;
    assert_eq!(r.status(), 200);
    let rows: Vec<Value> = r.json().await.unwrap();
    assert_eq!(rows.len(), 2);
    // Newest first, across both sources.
    assert_eq!(rows[0]["id"], "req-1");
    assert_eq!(rows[0]["kind"], "access");
    // A legacy row wears the same doc shape, kind quota, with its `requested` moved into `quota`.
    assert_eq!(rows[1]["id"], "qr-9");
    assert_eq!(rows[1]["kind"], "quota");
    assert_eq!(rows[1]["quota"]["cpu"], 12);
}

/// `?kind=` narrows to one queue; a legacy row is a quota row, so it drops out of every other.
#[tokio::test]
async fn the_kind_filter_narrows_both_sources() {
    let s = admin_server(vec![
        route_get(
            format!("{API}/requests"),
            list_of(
                "Request",
                vec![json!({
                    "metadata": {"name": "req-1", "creationTimestamp": "2026-09-04T10:00:00Z"},
                    "spec": {"owner": "acme", "kind": "access", "requestedBy": "meera", "reason": "r",
                             "access": {"team": "acme", "role": "admin"}},
                    "status": {"state": "pending"}
                })],
            ),
        ),
        route_get(format!("{API}/quotarequests"), list_of("QuotaRequest", vec![])),
    ])
    .await;
    let r = get(&format!("{}/admin/requests?kind=access", s.base), &admin_token(&s.jwt)).await;
    let rows: Vec<Value> = r.json().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "req-1");
}

// ── decisions ──────────────────────────────────────────────────────────────

async fn post(url: &str, token: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new().post(url).bearer_auth(token).json(&body).send().await.unwrap()
}

fn pending_quota_request(id: &str) -> Value {
    json!({"metadata": {"name": id, "creationTimestamp": "2026-09-04T10:00:00Z"},
           "spec": {"owner": "karthik", "kind": "quota", "requestedBy": "karthik", "reason": "r",
                    "quota": {"workspaces": 9}},
           "status": {"state": "pending"}})
}

fn pending_region_request() -> Value {
    json!({"metadata": {"name": "req-2", "creationTimestamp": "2026-09-04T10:00:00Z"},
           "spec": {"owner": "karthik", "kind": "region", "requestedBy": "karthik", "reason": "r",
                    "region": {"region": "westeurope-k3s"}},
           "status": {"state": "pending"}})
}

fn pending_other_request() -> Value {
    json!({"metadata": {"name": "req-3", "creationTimestamp": "2026-09-04T10:00:00Z"},
           "spec": {"owner": "karthik", "kind": "other", "requestedBy": "karthik", "reason": "r",
                    "other": {"title": "t", "body": "b"}},
           "status": {"state": "pending"}})
}

fn decided(id: &str, state: &str) -> Value {
    json!({"metadata": {"name": id}, "spec": {"owner": "karthik", "kind": "quota",
           "requestedBy": "karthik", "reason": "r", "quota": {"workspaces": 9}},
           "status": {"state": state}})
}

fn quota_object(regions: &[&str]) -> Value {
    json!({"metadata": {"name": "karthik"},
           "spec": {"workspaces": 5, "environments": 2, "snapshots": 20, "diskGb": 100,
                    "cpu": 8, "memoryGb": 32, "regions": regions}})
}

fn active_region() -> Value {
    json!({"metadata": {"name": "westeurope-k3s"}, "spec": {"name": "West Europe", "status": "active"}})
}

/// Quota approve is unchanged in substance: the Quota is written FIRST, then the request marked,
/// and the operator's edited values win over what was asked.
#[tokio::test]
async fn approving_a_quota_request_writes_the_quota_first() {
    let s = admin_server(vec![
        route_get(format!("{API}/requests/req-1"), pending_quota_request("req-1")),
        not_found(format!("{API}/quotas/karthik")),
        route_post(
            format!("{API}/quotas"),
            json!({"metadata": {"name": "karthik"}, "spec": {"workspaces": 12, "environments": 0,
                   "snapshots": 0, "diskGb": 0, "cpu": 0, "memoryGb": 0}}),
        ),
        route_patch(
            format!("{API}/requests/req-1/status"),
            decided("req-1", "approved"),
        ),
    ])
    .await;
    let r = post(
        &format!("{}/admin/requests/req-1/approve", s.base),
        &admin_token(&s.jwt),
        json!({"note": "ok", "quota": {"workspaces": 12}}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let written = s.rec.sent("POST", &format!("{API}/quotas"));
    assert_eq!(written[0]["spec"]["workspaces"], 12, "the operator's edit, not the asked-for 9");
}

/// Region approve records the grant on the owner's Quota and says so in `resolution` — the spec's
/// "a recorded decision only" has to be visible to the person who reads the decision back.
#[tokio::test]
async fn approving_a_region_request_records_the_grant() {
    let s = admin_server(vec![
        route_get(format!("{API}/requests/req-2"), pending_region_request()),
        route_get(format!("{API}/regions/westeurope-k3s"), active_region()),
        route_get(format!("{API}/quotas/karthik"), quota_object(&[])),
        route_patch(
            format!("{API}/quotas/karthik"),
            quota_object(&["westeurope-k3s"]),
        ),
        route_patch(
            format!("{API}/requests/req-2/status"),
            decided("req-2", "approved"),
        ),
    ])
    .await;
    let r = post(&format!("{}/admin/requests/req-2/approve", s.base), &admin_token(&s.jwt), json!({"note": "ok"})).await;
    assert_eq!(r.status(), 200);
    let patched = s.rec.sent("PATCH", &format!("{API}/quotas/karthik"));
    assert_eq!(patched[0]["spec"]["regions"], json!(["westeurope-k3s"]));
    // Read-modify-write, not a fresh spec: a region grant must not reset the limits already there.
    assert_eq!(patched[0]["spec"]["workspaces"], 5);
    let sent = s.rec.sent("PATCH", &format!("{API}/requests/req-2/status"));
    assert!(
        sent[0]["status"]["resolution"].as_str().unwrap().contains("recorded"),
        "the resolution has to say the grant is recorded, not enforced"
    );
}

/// An `other` request has nothing to write, so the free-text resolution IS the decision. Without
/// it, approve would mark a request done having done nothing at all.
#[tokio::test]
async fn approving_an_other_request_needs_a_resolution() {
    let s = admin_server(vec![route_get(format!("{API}/requests/req-3"), pending_other_request())]).await;
    let r = post(&format!("{}/admin/requests/req-3/approve", s.base), &admin_token(&s.jwt), json!({"note": "ok"})).await;
    assert_eq!(r.status(), 422);
    assert!(s.rec.sent("PATCH", &format!("{API}/requests/req-3/status")).is_empty());
}

/// Two admins racing: the second sees the decision, not a silent overwrite.
#[tokio::test]
async fn an_already_decided_request_is_a_conflict() {
    let s = admin_server(vec![route_get(format!("{API}/requests/req-4"), decided("req-4", "approved"))]).await;
    let r = post(&format!("{}/admin/requests/req-4/deny", s.base), &admin_token(&s.jwt), json!({"note": "no"})).await;
    assert_eq!(r.status(), 409);
}

/// Deny writes nothing but the mark, and the note is required — the asker has to be told why.
#[tokio::test]
async fn deny_requires_a_note() {
    let s = admin_server(vec![route_get(format!("{API}/requests/req-5"), pending_quota_request("req-5"))]).await;
    let r = post(&format!("{}/admin/requests/req-5/deny", s.base), &admin_token(&s.jwt), json!({})).await;
    assert_eq!(r.status(), 422);
}

/// A legacy `QuotaRequest` id decides through the SAME route: the console shows one queue, so it
/// must be able to act on every row in it without knowing which CRD the row came from.
#[tokio::test]
async fn a_legacy_id_decides_through_the_generic_route() {
    let s = admin_server(vec![
        route_get(
            format!("{API}/quotarequests/qr-9"),
            json!({"metadata": {"name": "qr-9"}, "spec": {"owner": "zoe", "requested": {"cpu": 12}, "reason": "old"},
                   "status": {"state": "pending"}}),
        ),
        not_found(format!("{API}/quotas/zoe")),
        route_post(
            format!("{API}/quotas"),
            json!({"metadata": {"name": "zoe"}, "spec": {"workspaces": 0, "environments": 0, "snapshots": 0,
                   "diskGb": 0, "cpu": 12, "memoryGb": 0}}),
        ),
        route_patch(
            format!("{API}/quotarequests/qr-9/status"),
            json!({"metadata": {"name": "qr-9"}, "spec": {"owner": "zoe", "requested": {"cpu": 12}, "reason": "old"},
                   "status": {"state": "approved"}}),
        ),
    ])
    .await;
    let r = post(&format!("{}/admin/requests/qr-9/approve", s.base), &admin_token(&s.jwt), json!({"note": "ok"})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(s.rec.sent("POST", &format!("{API}/quotas"))[0]["spec"]["cpu"], 12);
}

fn pending_access_request() -> Value {
    json!({"metadata": {"name": "req-6", "creationTimestamp": "2026-09-04T10:00:00Z"},
           "spec": {"owner": "meera", "kind": "access", "requestedBy": "meera", "reason": "r",
                    "access": {"team": "acme", "role": "admin"}},
           "status": {"state": "pending"}})
}

fn decided_access(state: &str) -> Value {
    json!({"metadata": {"name": "req-6"},
           "spec": {"owner": "meera", "kind": "access", "requestedBy": "meera", "reason": "r",
                    "access": {"team": "acme", "role": "admin"}},
           "status": {"state": state}})
}

/// The grant goes to the person who ASKED and the team they named — `spec.owner` is the asker's
/// own slug here, so granting on it would put them in a team of one named after themselves.
#[tokio::test]
async fn approving_access_grants_the_asker_into_the_named_team() {
    let s = admin_server_with(
        vec![
            route_get(format!("{API}/requests/req-6"), pending_access_request()),
            route_patch(format!("{API}/requests/req-6/status"), decided_access("approved")),
        ],
        StubMembership::answering(GrantAccess::Done),
    )
    .await;
    let r = post(&format!("{}/admin/requests/req-6/approve", s.base), &admin_token(&s.jwt), json!({"note": "ok"})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        s.dir.granted.lock().unwrap().as_slice(),
        [("acme".to_string(), "meera@example.com".to_string(), TeamRole::Admin)]
    );
    let sent = s.rec.sent("PATCH", &format!("{API}/requests/req-6/status"));
    assert!(sent[0]["status"]["resolution"].as_str().unwrap().contains("acme"));
}

/// Each directory answer has ONE status, and none of them marks the request decided: an approve
/// that did not grant must leave the row pending for somebody to retry.
#[tokio::test]
async fn a_refused_grant_maps_to_its_status_and_decides_nothing() {
    for (answer, code) in [
        (GrantAccess::NoSuchUser, 422),
        (GrantAccess::NoSuchTeam, 422),
        (GrantAccess::Refused("acme would have no owner left".into()), 409),
        (GrantAccess::Unsupported, 501),
    ] {
        let s = admin_server_with(
            vec![route_get(format!("{API}/requests/req-6"), pending_access_request())],
            StubMembership::answering(answer),
        )
        .await;
        let r =
            post(&format!("{}/admin/requests/req-6/approve", s.base), &admin_token(&s.jwt), json!({"note": "ok"})).await;
        assert_eq!(r.status(), code);
        assert!(s.rec.sent("PATCH", &format!("{API}/requests/req-6/status")).is_empty());
    }
}

/// An unknown role word is refused rather than rounded down to `member` — an approve that grants
/// something other than what was asked for is a false record.
#[tokio::test]
async fn an_unknown_role_is_refused() {
    let mut req = pending_access_request();
    req["spec"]["access"]["role"] = json!("superadmin");
    let s = admin_server_with(
        vec![route_get(format!("{API}/requests/req-6"), req)],
        StubMembership::answering(GrantAccess::Done),
    )
    .await;
    let r = post(&format!("{}/admin/requests/req-6/approve", s.base), &admin_token(&s.jwt), json!({"note": "ok"})).await;
    assert_eq!(r.status(), 422);
    assert!(s.dir.granted.lock().unwrap().is_empty());
}

/// Idempotent by uid: the new object's NAME is derived from the legacy object's uid, so a second
/// run finds it already there and copies nothing. An operator has to be able to run this twice.
#[tokio::test]
async fn the_migration_is_idempotent_by_uid() {
    let legacy = list_of(
        "QuotaRequest",
        vec![json!({
            "metadata": {"name": "qr-9", "uid": "7f1c1a2e-0000-4000-8000-000000000001",
                         "creationTimestamp": "2026-09-03T10:00:00Z"},
            "spec": {"owner": "zoe", "requested": {"cpu": 12}, "reason": "old"},
            "status": {"state": "pending"}
        })],
    );
    let s = admin_server(vec![
        route_get(format!("{API}/quotarequests"), legacy),
        // The API server answers a second create of the same name with 409 AlreadyExists — the
        // migration's own idempotence, not a check it does itself.
        route_conflict("POST", format!("{API}/requests")),
    ])
    .await;
    let r =
        post(&format!("{}/admin/requests/migrate", s.base), &admin_token(&s.jwt), json!({"note": "migrate"})).await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["copied"], 0);
    assert_eq!(body["skipped"], 1);
    let sent = s.rec.sent("POST", &format!("{API}/requests"));
    assert_eq!(sent[0]["metadata"]["name"], "q-7f1c1a2e-0000-4000-8000-000000000001");
    assert_eq!(sent[0]["spec"]["kind"], "quota");
    assert_eq!(sent[0]["spec"]["quota"]["cpu"], 12);
}

/// The grant carries the asker's HANDLE and the directory keys memberships on email, so a
/// handle the directory cannot resolve is `NoSuchUser` — not a membership written for a name
/// nobody answers to.
#[tokio::test]
async fn an_unresolvable_handle_is_no_such_user() {
    let mut req = pending_access_request();
    req["spec"]["requestedBy"] = json!("nobody");
    let s = admin_server_with(
        vec![route_get(format!("{API}/requests/req-6"), req)],
        StubMembership::answering(GrantAccess::Done),
    )
    .await;
    let r = post(&format!("{}/admin/requests/req-6/approve", s.base), &admin_token(&s.jwt), json!({"note": "ok"})).await;
    assert_eq!(r.status(), 422);
    assert!(s.rec.sent("PATCH", &format!("{API}/requests/req-6/status")).is_empty());
}

/// Create and status stamp are two calls, so a run that died between them left a decided legacy
/// request pending in the new queue forever. A re-run has to finish the job it started.
#[tokio::test]
async fn a_rerun_stamps_a_copy_whose_status_never_landed() {
    let uid = "7f1c1a2e-0000-4000-8000-000000000001";
    let legacy = list_of(
        "QuotaRequest",
        vec![json!({
            "metadata": {"name": "qr-9", "uid": uid, "creationTimestamp": "2026-09-03T10:00:00Z"},
            "spec": {"owner": "zoe", "requested": {"cpu": 12}, "reason": "old"},
            "status": {"state": "approved", "decidedBy": "root@example.com", "note": "ok"}
        })],
    );
    let copy = json!({"metadata": {"name": format!("q-{uid}")},
                      "spec": {"owner": "zoe", "kind": "quota", "requestedBy": "zoe", "reason": "old",
                               "quota": {"cpu": 12}},
                      "status": {"state": "pending"}});
    let s = admin_server(vec![
        route_get(format!("{API}/quotarequests"), legacy),
        route_conflict("POST", format!("{API}/requests")),
        route_get(format!("{API}/requests/q-{uid}"), copy),
        route_patch(format!("{API}/requests/q-{uid}/status"), decided_access("approved")),
    ])
    .await;
    let r =
        post(&format!("{}/admin/requests/migrate", s.base), &admin_token(&s.jwt), json!({"note": "migrate"})).await;
    assert_eq!(r.status(), 200);
    let sent = s.rec.sent("PATCH", &format!("{API}/requests/q-{uid}/status"));
    assert_eq!(sent[0]["status"]["state"], "approved");
    assert_eq!(sent[0]["status"]["decidedBy"], "root@example.com");
}

/// Team scoping and the team-admin check both compare a `members.user` entry, and those hold
/// emails while every caller here holds a handle — so the resolution has to happen before the
/// comparison, or a real member reads as belonging to nothing.
#[tokio::test]
async fn a_handle_resolves_to_the_membership_held_under_its_email() {
    let dir = StubMembership::answering(GrantAccess::Done);
    assert_eq!(dir.teams_for("meera").await, vec!["acme".to_string()]);
    assert_eq!(dir.team_role("meera", "acme").await, Some(TeamRole::Member));
    // The email is not an identity any caller holds; passing one through must not match either.
    assert!(dir.teams_for("meera@example.com").await.is_empty());
    assert!(dir.team_role("meera@example.com", "acme").await.is_none());
}

/// A directory that cannot be read is not a verdict on the asker: it is a retryable 409, never
/// the 422 that tells a superadmin to go looking for a person who exists.
#[tokio::test]
async fn an_unreadable_directory_is_a_conflict_not_no_such_user() {
    let dir = StubMembership { unreadable: true, ..StubMembership::answering(GrantAccess::Done) };
    let s =
        admin_server_with(vec![route_get(format!("{API}/requests/req-6"), pending_access_request())], dir).await;
    let r = post(&format!("{}/admin/requests/req-6/approve", s.base), &admin_token(&s.jwt), json!({"note": "ok"})).await;
    assert_eq!(r.status(), 409);
    assert!(s.rec.sent("PATCH", &format!("{API}/requests/req-6/status")).is_empty());
}
