# Generic Requests Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One cluster-scoped `Request` CRD with kinds quota / access / region / other, raised by users through `/v1` and decided by superadmins through the admin process, replacing the quota-only `QuotaRequest`.

**Architecture:** `Request` is a new CRD next to `QuotaRequest` in `crates/workspaces/src/crd/mod.rs`: `spec.owner` is truth, `spec.kind` picks which of four optional per-kind blocks must be present, and `/v1` writes spec + the pending status exactly as it does today. `bins/api` in the default `user` role gets `/v1/requests` (create / list mine / get) and keeps `POST /v1/quota-requests` as a thin wrapper that writes a kind-quota `Request`; `bins/api` in `admin` role gets `GET /admin/requests` (unioning `Request` and legacy `QuotaRequest` rows into one `RequestDoc`) and `POST /admin/requests/{id}/approve|deny`, whose approve arm is kind-specific: quota writes the `Quota` through the existing single writer `write_quota`, access sets team membership through the directory the admin process already holds, region appends to `Quota.spec.regions`, other requires a free-text resolution. Every decision writes the consequential thing first, then audits, then marks the request — the order the quota path already uses.

**Tech Stack:** Rust (axum, kube-rs `CustomResource`, serde), Kubernetes CRDs + RBAC in `deploy/k3s/`, Next.js app router + server actions in `web/apps/web`, bash for the e2e.

**Spec:** `docs/superpowers/specs/2026-09-04-history-and-console-v2-design.md` (§B "Generic requests"; §A and §C are separate sub-projects and are NOT built here)

## Global Constraints

- **`spec.owner` is truth; labels are a view, never authorization.** `kloudlite-git.io/owner` is stamped only so a list can be selector-narrowed, and every list re-filters on `spec.owner`.
- **`/v1` writes spec; controllers write status.** `Request` is the documented exception `QuotaRequest` already is: no controller reconciles a request, so the API tier writes its status too. Nothing else changes about the split.
- **Only a superadmin decides.** Decision routes live only in `api::admin::router()`, behind `refuse_without_claim`; the user router has no decision handler compiled into it at all.
- **One pending request per owner PER KIND** — a second is `409 "a request is already pending"`.
- **RBAC mirrors the split:** `kloudlite-git-api` gets `get, list, create` on `requests`; `kloudlite-git-admin` gets `get, list, create, patch, delete` on `requests` and `patch, update` on `requests/status`.
- **`deploy/k3s/crds.yaml` is GENERATED** — never hand-edit; regenerate with `CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml`.
- **Audit immediately after the consequential write**, never after a second fallible call; refusals of 409 and up go through `audited`.
- Audit action words for decisions: `request.approved` / `request.denied`.
- Comments explain WHY, never what. Commit subjects are imperative sentence case, no tool attribution.
- `cargo clippy --workspace -- -D warnings` must stay clean.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/workspaces/src/crd/mod.rs` (modify) | `Request`, `RequestSpec`, `RequestKind`, `AccessAsk`, `RegionAsk`, `OtherAsk`, `RequestStatus`, `RequestSpec::validate`; `QuotaSpec.regions`; `Request::crd()` in `all_crds()` |
| `crates/workspaces/src/api/mod.rs` (modify) | `/v1/requests` create/list/get, the `/v1/quota-requests` wrapper, `RequestDoc`, `Directory::grant_access` |
| `crates/workspaces/src/api/admin.rs` (modify) | `GET /admin/requests`, `POST /admin/requests/{id}/approve|deny`, the four kind arms, `POST /admin/requests/migrate` |
| `bins/api/src/main.rs` (modify) | `Dir::grant_access` — the mongo directory's add/set-role behind the workspaces trait |
| `deploy/k3s/crds.yaml` (regenerate), `deploy/k3s/api-rbac.yaml` (modify) | install + authority |
| `web/apps/web/src/lib/requests.ts` (create) | kinds, labels, the `RequestDoc` shape's client helpers |
| `web/apps/web/src/lib/api.ts` (modify) | `RequestDoc`, `createRequest`, `listRequests`, `adminListRequests`, `adminDecideRequest` |
| `web/apps/web/src/app/(shell)/requests/*` (create) | "My requests" page + the new-request action |
| `web/apps/web/src/components/app/new-request-dialog.tsx` (create) | kind picker + four small forms |
| `tests/ws_e2e.sh` (modify) | create + approve of an access request end to end |

---

### Task 1: The `Request` CRD

**Files:**
- Modify: `crates/workspaces/src/crd/mod.rs` (after `RequestState`, ~line 965)
- Modify: `deploy/k3s/crds.yaml` (regenerated, never hand-edited)
- Modify: `deploy/k3s/api-rbac.yaml`
- Test: `crates/workspaces/src/crd/mod.rs` (`#[cfg(test)] mod request_tests`), `crates/workspaces/tests/crd_yaml.rs` (existing drift check)

**Interfaces:**
- Produces: `crd::Request` (kube `CustomResource`, cluster-scoped, plural `requests`, shortname `req`), `crd::RequestSpec { owner, kind, requested_by, reason, quota, access, region, other }`, `crd::RequestKind::{Quota, Access, Region, Other}`, `crd::AccessAsk { team, role }`, `crd::RegionAsk { region }`, `crd::OtherAsk { title, body }`, `crd::RequestStatus { state, decided_by, decided_at, note, resolution }`, `crd::RequestSpec::validate(&self) -> Result<(), String>`, `crd::QuotaSpec.regions: Vec<String>`.
- Consumes: existing `crd::RequestState`, `crd::RequestedQuota`, `crd::Condition`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/workspaces/src/crd/mod.rs`:

```rust
#[cfg(test)]
mod request_tests {
    use super::*;

    fn base(kind: RequestKind) -> RequestSpec {
        RequestSpec {
            owner: "acme".into(),
            kind,
            requested_by: "meera".into(),
            reason: "more room".into(),
            quota: None,
            access: None,
            region: None,
            other: None,
        }
    }

    /// The wire form is what an operator reads with `kubectl get request -o yaml`, and what a
    /// stored object parses back from — both directions, so a rename cannot pass unnoticed.
    #[test]
    fn a_request_round_trips_through_its_wire_form() {
        let mut spec = base(RequestKind::Access);
        spec.access = Some(AccessAsk { team: "acme".into(), role: "admin".into() });
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["kind"], "access");
        assert_eq!(v["requestedBy"], "meera");
        assert_eq!(v["access"]["role"], "admin");
        // Blocks for the other three kinds are absent, not null: a null would advertise a field
        // the request never carried.
        assert!(v.get("quota").is_none() && v.get("region").is_none() && v.get("other").is_none());
        assert_eq!(serde_json::from_value::<RequestSpec>(v).unwrap(), spec);
    }

    /// A request carrying somebody else's block is not a typo to tolerate: `approve` dispatches on
    /// `kind` and would silently ignore the block that was actually filled in.
    #[test]
    fn exactly_the_block_for_its_kind_must_be_present() {
        let mut ok = base(RequestKind::Quota);
        ok.quota = Some(RequestedQuota { workspaces: Some(9), ..Default::default() });
        assert_eq!(ok.validate(), Ok(()));

        let missing = base(RequestKind::Quota);
        assert_eq!(missing.validate(), Err("kind quota needs a quota block".to_string()));

        let mut extra = base(RequestKind::Quota);
        extra.quota = Some(RequestedQuota::default());
        extra.other = Some(OtherAsk { title: "t".into(), body: "b".into() });
        assert_eq!(extra.validate(), Err("only the quota block belongs on a quota request".to_string()));

        let mut wrong = base(RequestKind::Region);
        wrong.access = Some(AccessAsk { team: "acme".into(), role: "admin".into() });
        assert_eq!(wrong.validate(), Err("kind region needs a region block".to_string()));
    }

    /// Only the three directory roles; anything else would reach `grant_access` as a role nothing
    /// can map, and a 500 on approve is a decision that half-happened.
    #[test]
    fn an_access_request_takes_only_a_real_role() {
        let mut spec = base(RequestKind::Access);
        spec.access = Some(AccessAsk { team: "acme".into(), role: "superuser".into() });
        assert_eq!(spec.validate(), Err("role must be member, admin or owner".to_string()));
    }

    /// `regions` is a granted list, and an empty one is omitted so a merge patch of a `QuotaSpec`
    /// that never mentions regions (every `PUT /admin/quota/{owner}` body) cannot erase a grant.
    #[test]
    fn an_empty_region_grant_is_omitted_from_a_quota_patch() {
        let v = serde_json::to_value(default_quota(false)).unwrap();
        assert!(v.get("regions").is_none());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces --lib request_tests`
Expected: FAIL — `cannot find type RequestSpec in this scope` / `RequestKind` not found.

- [ ] **Step 3: Add the types**

In `crates/workspaces/src/crd/mod.rs`, add `regions` to `QuotaSpec` (right after `memory_gb`):

```rust
    /// Regions this owner has been GRANTED beyond whatever placement offers by default. Recorded
    /// here, on the one per-owner cluster-scoped object the admin process already owns, rather
    /// than on an `OwnerBinding` — a binding is per `{owner, region}` and is authored by the
    /// claiming agent, so a per-owner grant list has no coherent home there. Nothing reads it for
    /// placement yet (spec §B: "a recorded decision only"); per-owner region gating lands later
    /// and reads exactly this field.
    ///
    /// Skipped when empty on purpose: `write_quota` merge-patches a whole `QuotaSpec`, and
    /// `PUT /admin/quota/{owner}` bodies never mention regions — serializing `[]` would erase a
    /// grant every time somebody edited a limit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
```

Update the two `QuotaSpec { .. }` literals in `default_quota` to add `regions: Vec::new()` to each arm.

Then, after `RequestState`, add:

```rust
/// What a person is asking for. One CRD for all four kinds, because the LIFECYCLE is identical —
/// opened by a user, one pending at a time, decided by a superadmin, kept forever as the record —
/// and only the payload and what approve DOES differ. Four CRDs would have meant four RBAC rules,
/// four list routes and four console tables for one workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RequestKind {
    Quota,
    Access,
    Region,
    Other,
}

impl RequestKind {
    /// The wire word, for a filter query and for the audit target — one spelling, so a URL's
    /// `?kind=` and a stored object can never disagree.
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestKind::Quota => "quota",
            RequestKind::Access => "access",
            RequestKind::Region => "region",
            RequestKind::Other => "other",
        }
    }
}

/// Join a team, or move to a different role in one. `role` is the directory's own word
/// (`member` / `admin` / `owner`) rather than an enum, because the directory's `Role` lives in
/// `kloudlite-git-pulls` and this crate deliberately does not depend on it; `validate` is what stops
/// a typo reaching the grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccessAsk {
    pub team: String,
    pub role: String,
}

pub const ROLES: [&str; 3] = ["member", "admin", "owner"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegionAsk {
    pub region: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OtherAsk {
    pub title: String,
    pub body: String,
}

/// A person asking for something, and the decision on it. Supersedes `QuotaRequest`, which stays
/// readable until the one-shot migration has run everywhere and a later release retires it.
///
/// Like `QuotaRequest`, this is the one shape whose STATUS the API tier writes rather than a
/// controller: no controller reconciles a request — a person decides it — so the decision has
/// nowhere else to live.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite-git.io",
    version = "v1alpha1",
    kind = "Request",
    plural = "requests",
    shortname = "req",
    status = "RequestStatus",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Kind","type":"string","jsonPath":".spec.kind"}"#,
    printcolumn = r#"{"name":"State","type":"string","jsonPath":".status.state"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct RequestSpec {
    /// The person or team the request is FOR — truth, never a label. For an access request this
    /// is the asker's own slug and `access.team` names the team they want into: the team is what
    /// they do not have yet, so it cannot also be the owner that authorizes the ask.
    pub owner: String,
    pub kind: RequestKind,
    /// The signed-in user who opened it. Set by `/v1` from the caller's claims, never from the
    /// body — a request that could name its own author is not evidence of anything.
    pub requested_by: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<RequestedQuota>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<AccessAsk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<RegionAsk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<OtherAsk>,
}

impl RequestSpec {
    /// Exactly the block for its kind, and nothing else. `approve` dispatches on `kind`, so a
    /// request carrying a second block would have a payload the decision silently ignores — and
    /// a request carrying none would be approved into a no-op.
    pub fn validate(&self) -> Result<(), String> {
        let present = [
            ("quota", self.quota.is_some()),
            ("access", self.access.is_some()),
            ("region", self.region.is_some()),
            ("other", self.other.is_some()),
        ];
        let want = self.kind.as_str();
        for (name, is_set) in present {
            if is_set && name != want {
                return Err(format!("only the {want} block belongs on a {want} request"));
            }
            if !is_set && name == want {
                return Err(format!("kind {want} needs a {want} block"));
            }
        }
        if let Some(a) = &self.access {
            if !ROLES.contains(&a.role.as_str()) {
                return Err("role must be member, admin or owner".to_string());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestStatus {
    pub state: RequestState,
    /// The deciding superadmin's email, for the audit trail. Never an owner of anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// What approve actually DID, in one sentence — the quota that was written, the role that was
    /// set, the recorded region grant, or the free text a superadmin typed for an `other`. Kept
    /// separately from `note` because the note is the decider's message to the asker and this is
    /// the record of the effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}
```

Add `Request::crd(),` to `all_crds()`, after `QuotaRequest::crd()`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces --lib request_tests`
Expected: PASS (4 tests)

- [ ] **Step 5: Regenerate the CRD manifest and confirm the drift check holds**

```bash
CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml
cargo test -p kloudlite-git-workspaces --test crd_yaml
```
Expected: the second run PASSES, and `git diff --stat deploy/k3s/crds.yaml` is non-empty.

- [ ] **Step 6: Grant the two ServiceAccounts their authority**

In `deploy/k3s/api-rbac.yaml`, in the `kloudlite-git-api` ClusterRole, directly after the `quotarequests` rule:

```yaml
  # The generic request queue. Same reach as `quotarequests` and for the same reason: a person or
  # a team admin may OPEN one for themselves, and deciding it is not a /v1 verb — no `patch`, and
  # no `requests/status` at all, so this role cannot mark its own request approved.
  - apiGroups: ["kloudlite-git.io"]
    resources: ["requests"]
    verbs: ["get", "list", "create"]
```

And in the `kloudlite-git-admin` ClusterRole, directly after the `quotarequests/status` rule:

```yaml
  # Deciding a request: `patch` on the object for the migration's label repair, `patch`/`update` on
  # the status subresource for the decision itself. `create` is here because the one-shot migration
  # (`POST /admin/requests/migrate`) authors a Request per legacy QuotaRequest.
  - apiGroups: ["kloudlite-git.io"]
    resources: ["requests"]
    verbs: ["get", "list", "create", "patch", "delete"]
  - apiGroups: ["kloudlite-git.io"]
    resources: ["requests/status"]
    verbs: ["patch", "update"]
```

No change to `deploy/k3s/agent-admission.yaml`: the policy binds the agent's ServiceAccount to the resources a controller reconciles, and no controller ever touches a `Request`.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/crd/mod.rs deploy/k3s/crds.yaml deploy/k3s/api-rbac.yaml
git commit -m "Add a generic Request CRD with per-kind payload blocks"
```

---

### Task 2: `/v1/requests` — open, list mine, get

**Files:**
- Modify: `crates/workspaces/src/api/mod.rs:248` (router), and the quota-request section at `crates/workspaces/src/api/mod.rs:340-447`
- Test: `crates/workspaces/tests/api_requests.rs` (create)

**Interfaces:**
- Consumes: `crd::Request`, `crd::RequestSpec`, `crd::RequestKind`, `crd::RequestSpec::validate` (Task 1); existing `caller`, `scope::may_act_on`, `scope::caller_owners`, `scope::owned_by`, `may_request_for`, `rid`, `check_region`, `OWNER_LABEL`.
- Produces: `pub(crate) fn generic_doc(r: &crd::Request) -> RequestDoc`, `pub(crate) struct RequestDoc` (serialized camelCase: `id, owner, kind, requestedBy, reason, quota, access, region, other, state, decidedBy, decidedAt, note, resolution, createdAt`), `pub(crate) async fn requests_of_generic(c: &kube::Client, owner: &str) -> Result<Vec<crd::Request>, Response>`, `pub(crate) fn is_pending_generic(r: &crd::Request) -> bool`, `pub(crate) async fn create_request_inner(s: &ApiState, caller: &Caller, spec: crd::RequestSpec) -> Result<crd::Request, Response>`.

- [ ] **Step 1: Write the failing test**

Create `crates/workspaces/tests/api_requests.rs`:

```rust
//! `/v1/requests` against a mocked API server. The stub `Directory` is the one `api_quota.rs`
//! uses: `karthik` is an admin of team `acme`, `bob` a plain member.

use kloudlite_git_core::jwt::Jwt;
use kloudlite_git_workspaces::api::{router, ApiState, Directory, TeamRole};
use kloudlite_git_workspaces::kube_test::{get, mock_client, not_found, post, Recorder, Route};
use serde_json::{json, Value};
use std::sync::Arc;

const API: &str = "/apis/kloudlite-git.io/v1alpha1";

struct StubMembership;

#[async_trait::async_trait]
impl Directory for StubMembership {
    async fn teams_for(&self, user: &str) -> Vec<String> {
        if user == "karthik" || user == "bob" { vec!["acme".into()] } else { vec![] }
    }
    async fn team_role(&self, user: &str, team: &str) -> Option<TeamRole> {
        match (user, team) {
            ("karthik", "acme") => Some(TeamRole::Admin),
            ("bob", "acme") => Some(TeamRole::Member),
            _ => None,
        }
    }
    async fn is_live(&self, _jti: &str) -> bool {
        false
    }
    async fn for_owner(&self, _owner: &str) -> Option<kloudlite_git_workspaces::api::OwnerMaterial> {
        None
    }
    async fn is_team(&self, slug: &str) -> bool {
        slug == "acme"
    }
}

struct Server {
    base: String,
    jwt: Arc<Jwt>,
    #[allow(dead_code)]
    rec: Recorder,
}

async fn server(routes: Vec<Route>) -> Server {
    let jwt = Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap());
    let (client, rec) = mock_client(routes);
    let state = ApiState::new(jwt.clone()).with_directory(Arc::new(StubMembership)).with_kube(client);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router(Arc::new(state))).await.unwrap() });
    Server { base, jwt, rec }
}

fn token(jwt: &Jwt, user: &str) -> String {
    jwt.mint_user(user, user, false).unwrap()
}

fn list_of(items: Vec<Value>) -> Value {
    json!({"apiVersion": "v1", "kind": "RequestList", "items": items})
}

fn stored(id: &str, owner: &str, kind: &str, state: Option<&str>) -> Value {
    let mut o = json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Request",
        "metadata": {"name": id, "labels": {"kloudlite-git.io/owner": owner}},
        "spec": {"owner": owner, "kind": kind, "requestedBy": owner, "reason": "r",
                 "other": {"title": "t", "body": "b"}},
    });
    if let Some(st) = state {
        o["status"] = json!({"state": st});
    }
    o
}

/// The signed-in caller is the author, whatever the body says: `requestedBy` is evidence.
#[tokio::test]
async fn a_create_takes_its_author_from_the_claims() {
    let s = server(vec![
        Route::new("GET", &format!("{API}/requests"), list_of(vec![])),
        Route::new("POST", &format!("{API}/requests"), stored("req-1", "karthik", "other", None)),
    ])
    .await;
    let r = post(
        &format!("{s}/v1/requests", s = s.base),
        &token(&s.jwt, "karthik"),
        json!({"kind": "other", "reason": "r", "other": {"title": "t", "body": "b"},
               "requestedBy": "someone-else"}),
    )
    .await;
    assert_eq!(r.status(), 201);
    let sent = s.rec.body("POST", &format!("{API}/requests")).unwrap();
    assert_eq!(sent["spec"]["requestedBy"], "karthik");
    assert_eq!(sent["spec"]["owner"], "karthik");
    assert_eq!(sent["metadata"]["labels"]["kloudlite-git.io/owner"], "karthik");
}

/// One pending per owner PER KIND: a pending `other` must not block a `quota`.
#[tokio::test]
async fn one_pending_per_owner_per_kind() {
    let s = server(vec![
        Route::new(
            "GET",
            &format!("{API}/requests"),
            list_of(vec![stored("req-1", "karthik", "other", Some("pending"))]),
        ),
        Route::new("POST", &format!("{API}/requests"), stored("req-2", "karthik", "quota", None)),
    ])
    .await;
    let same = post(
        &format!("{s}/v1/requests", s = s.base),
        &token(&s.jwt, "karthik"),
        json!({"kind": "other", "reason": "again", "other": {"title": "t", "body": "b"}}),
    )
    .await;
    assert_eq!(same.status(), 409, "a second pending request of the same kind is refused");

    let other_kind = post(
        &format!("{s}/v1/requests", s = s.base),
        &token(&s.jwt, "karthik"),
        json!({"kind": "quota", "reason": "room", "quota": {"workspaces": 9}}),
    )
    .await;
    assert_eq!(other_kind.status(), 201, "a different kind is a different queue");
}

/// A malformed request never reaches the cluster: the block has to match the kind.
#[tokio::test]
async fn a_block_that_does_not_match_the_kind_is_refused() {
    let s = server(vec![Route::new("GET", &format!("{API}/requests"), list_of(vec![]))]).await;
    let r = post(
        &format!("{s}/v1/requests", s = s.base),
        &token(&s.jwt, "karthik"),
        json!({"kind": "quota", "reason": "r", "other": {"title": "t", "body": "b"}}),
    )
    .await;
    assert_eq!(r.status(), 422);
    assert!(s.rec.body("POST", &format!("{API}/requests")).is_none(), "nothing was written");
}

/// A plain member cannot open a request against the team's ceiling — the same directory rule
/// `/v1/quota-requests` already applies, unchanged by the new kinds.
#[tokio::test]
async fn only_a_team_admin_may_ask_for_a_team() {
    let s = server(vec![Route::new("GET", &format!("{API}/requests"), list_of(vec![]))]).await;
    let r = post(
        &format!("{s}/v1/requests", s = s.base),
        &token(&s.jwt, "bob"),
        json!({"owner": "acme", "kind": "quota", "reason": "r", "quota": {"cpu": 9}}),
    )
    .await;
    assert_eq!(r.status(), 403);
}

/// `GET /v1/requests/{id}` is the caller's own, and somebody else's is a 404 — never a 403,
/// which would confirm the id exists.
#[tokio::test]
async fn another_owners_request_is_not_found() {
    let s = server(vec![Route::new(
        "GET",
        &format!("{API}/requests/req-9"),
        stored("req-9", "zoe", "other", Some("pending")),
    )])
    .await;
    let r = get(&format!("{s}/v1/requests/req-9", s = s.base), &token(&s.jwt, "karthik")).await;
    assert_eq!(r.status(), 404);
}
```

Check the helper signatures in `crates/workspaces/src/kube_test.rs` first (`Route::new`, `Recorder::body`, `get`, `post`, `mock_client`, and the jwt mint helper `api_quota.rs` uses) and match them exactly — copy the local spelling rather than the one above if they differ.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --test api_requests`
Expected: FAIL — every case 404s, the routes do not exist.

- [ ] **Step 3: Add the routes**

In `crates/workspaces/src/api/mod.rs`'s `router()`, after the `/v1/quota-requests` line:

```rust
        .route("/v1/requests", post(create_request).get(list_requests))
        .route("/v1/requests/{id}", get(get_request))
```

And in the quota-request section, after `list_quota_requests`:

```rust
/// The generic queue's own doc. One shape for all four kinds — the block that is `None` is simply
/// absent, so a console renders "the facts for this kind" by reading the one field that is set.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestDoc {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) kind: crd::RequestKind,
    pub(crate) requested_by: String,
    pub(crate) reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) quota: Option<crd::RequestedQuota>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) access: Option<crd::AccessAsk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) region: Option<crd::RegionAsk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) other: Option<crd::OtherAsk>,
    pub(crate) state: crd::RequestState,
    pub(crate) decided_by: Option<String>,
    pub(crate) decided_at: Option<String>,
    pub(crate) note: Option<String>,
    pub(crate) resolution: Option<String>,
    pub(crate) created_at: Option<String>,
}

pub(crate) fn generic_doc(r: &crd::Request) -> RequestDoc {
    let st = r.status.clone().unwrap_or_default();
    RequestDoc {
        id: r.name_any(),
        owner: r.spec.owner.clone(),
        kind: r.spec.kind,
        requested_by: r.spec.requested_by.clone(),
        reason: r.spec.reason.clone(),
        quota: r.spec.quota.clone(),
        access: r.spec.access.clone(),
        region: r.spec.region.clone(),
        other: r.spec.other.clone(),
        state: st.state,
        decided_by: st.decided_by,
        decided_at: st.decided_at,
        note: st.note,
        resolution: st.resolution,
        created_at: r.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()),
    }
}

/// Every request of `owner`, label-selected — and re-checked against `spec.owner`, because the
/// label is a view.
pub(crate) async fn requests_of_generic(c: &kube::Client, owner: &str) -> Result<Vec<crd::Request>, Response> {
    let api: Api<crd::Request> = Api::all(c.clone());
    Ok(api
        .list(&scope::owned_by(owner))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|r| r.spec.owner == owner)
        .collect())
}

/// No status yet is PENDING — `/v1` writes the object and stamps status in a second call, and
/// reading that window as "decided" would let two requests of one kind stand at once.
pub(crate) fn is_pending_generic(r: &crd::Request) -> bool {
    r.status.as_ref().map(|s| s.state).unwrap_or_default() == crd::RequestState::Pending
}

#[derive(serde::Deserialize)]
struct NewRequest {
    /// Absent means the caller's own.
    #[serde(default)]
    owner: Option<String>,
    kind: crd::RequestKind,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    quota: Option<crd::RequestedQuota>,
    #[serde(default)]
    access: Option<crd::AccessAsk>,
    #[serde(default)]
    region: Option<crd::RegionAsk>,
    #[serde(default)]
    other: Option<crd::OtherAsk>,
}

/// The one place a `Request` is authored. Shared with the `/v1/quota-requests` wrapper so the
/// per-kind pending rule, the label and the author cannot be spelled twice.
pub(crate) async fn create_request_inner(
    s: &ApiState,
    caller: &Caller,
    spec: crd::RequestSpec,
) -> Result<crd::Request, Response> {
    spec.validate().map_err(|m| (StatusCode::UNPROCESSABLE_ENTITY, m).into_response())?;
    may_request_for(s, &caller.name, &spec.owner).await?;
    // A region has to be one an admin registered and left active — approving a grant for a region
    // that does not exist would record a decision nothing can ever honour.
    if let Some(r) = &spec.region {
        check_region(s, &r.region).await?;
    }
    let client = kube(s)?;
    // One at a time PER KIND, so each queue is a list of decisions rather than a list of the
    // same ask — and a pending access request never blocks an unrelated quota one.
    if requests_of_generic(client, &spec.owner)
        .await?
        .iter()
        .any(|r| is_pending_generic(r) && r.spec.kind == spec.kind)
    {
        return Err((StatusCode::CONFLICT, "a request is already pending").into_response());
    }
    let owner = spec.owner.clone();
    let mut r = crd::Request::new(&rid("req"), spec);
    // A view of `spec.owner`, so the queue and the owner's own list are indexed selectors — same
    // rule as every other label in this codebase.
    r.metadata.labels = Some(std::collections::BTreeMap::from([(OWNER_LABEL.to_string(), owner)]));
    let api: Api<crd::Request> = Api::all(client.clone());
    api.create(&kube::api::PostParams::default(), &r).await.map_err(kube_err)
}

async fn create_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewRequest>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let spec = crd::RequestSpec {
        owner: body.owner.unwrap_or_else(|| c.name.clone()),
        kind: body.kind,
        // From the claims, never the body: an author a request could name for itself is not
        // evidence of who asked.
        requested_by: c.name.clone(),
        reason: body.reason,
        quota: body.quota,
        access: body.access,
        region: body.region,
        other: body.other,
    };
    let made = create_request_inner(&s, &c, spec).await?;
    Ok((StatusCode::CREATED, Json(generic_doc(&made))).into_response())
}

/// The caller's own requests and their teams'. `owner` narrows to one, and must be something the
/// caller may act on — same rule as every other owner-scoped read.
async fn list_requests(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<RequestQuery>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let client = kube(&s)?;
    let mut rows = Vec::new();
    match q.owner {
        Some(owner) => {
            if !scope::may_act_on(&s, &c, &owner).await {
                return Err(not_found());
            }
            rows.extend(requests_of_generic(client, &owner).await?);
        }
        None => {
            for owner in scope::caller_owners(&s, &c).await {
                rows.extend(requests_of_generic(client, &owner).await?);
            }
        }
    }
    rows.sort_by(|a, b| b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp));
    Ok(Json(rows.iter().map(generic_doc).collect::<Vec<_>>()).into_response())
}

async fn get_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    check_path_segment(&id)?;
    let api: Api<crd::Request> = Api::all(kube(&s)?.clone());
    let r = api.get_opt(&id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    // 404, never 403: a refusal that distinguishes "not yours" from "no such id" confirms the id.
    if !scope::may_act_on(&s, &c, &r.spec.owner).await {
        return Err(not_found());
    }
    Ok(Json(generic_doc(&r)).into_response())
}
```

Add `use axum::extract::Path;` to the module's imports if it is not already there (check the top of `api/mod.rs`; `Path` is used by the workspace routes, so it usually is).

- [ ] **Step 4: Point the quota-request create at the new CRD**

Replace the body of `create_quota_request` (`crates/workspaces/src/api/mod.rs:360`) with the wrapper, leaving `list_quota_requests` and `requests_of`/`is_pending` alone — the list still has legacy rows to serve:

```rust
/// The pre-`Request` route, kept because the web's 409 dialog and `kl` both post here. It writes a
/// kind-quota `Request` now: one queue, one pending rule, one decision path — the old CRD is only
/// ever READ from here on.
async fn create_quota_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewQuotaRequest>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let spec = crd::RequestSpec {
        owner: body.owner.unwrap_or_else(|| c.name.clone()),
        kind: crd::RequestKind::Quota,
        requested_by: c.name.clone(),
        reason: body.reason,
        quota: Some(body.requested),
        access: None,
        region: None,
        other: None,
    };
    let made = create_request_inner(&s, &c, spec).await?;
    Ok((StatusCode::CREATED, Json(generic_doc(&made))).into_response())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces --test api_requests --test api_quota`
Expected: PASS. If `api_quota.rs` has a case asserting `POST /v1/quota-requests` hits `.../quotarequests`, update that route expectation to `.../requests` and its assertion comment — the wrapper is the point.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/api/mod.rs crates/workspaces/tests/api_requests.rs crates/workspaces/tests/api_quota.rs
git commit -m "Open generic requests from /v1 and route quota asks through them"
```

---

### Task 3: `GET /admin/requests` — one queue over both CRDs

**Files:**
- Modify: `crates/workspaces/src/api/admin.rs` (router at :120, `RequestFilter` at :346)
- Test: `crates/workspaces/tests/api_admin_requests.rs` (create)

**Interfaces:**
- Consumes: `RequestDoc`, `generic_doc` (Task 2); existing `RequestFilter`, `list_all_quota_requests_inner`.
- Produces: `pub(crate) async fn list_requests_inner(s: &ApiState, f: &RequestFilter) -> Result<Vec<RequestDoc>, Response>` — the union, newest first; `RequestFilter` grows `kind: Option<crd::RequestKind>`.

- [ ] **Step 1: Write the failing test**

Create `crates/workspaces/tests/api_admin_requests.rs`. Copy the admin harness from `crates/workspaces/tests/api_admin.rs` verbatim (its `server()` builds an `ApiState` and mounts `api::admin::router()`, and its token helper mints a `superadmin: true` claim) and add:

```rust
/// The queue is one list over two CRDs while the legacy objects still exist: a console must not
/// have to know that a migration is half-done.
#[tokio::test]
async fn the_queue_unions_legacy_quota_requests() {
    let s = admin_server(vec![
        Route::new(
            "GET",
            &format!("{API}/requests"),
            json!({"items": [{
                "metadata": {"name": "req-1", "creationTimestamp": "2026-09-04T10:00:00Z"},
                "spec": {"owner": "acme", "kind": "access", "requestedBy": "meera", "reason": "r",
                         "access": {"team": "acme", "role": "admin"}},
                "status": {"state": "pending"}
            }]}),
        ),
        Route::new(
            "GET",
            &format!("{API}/quotarequests"),
            json!({"items": [{
                "metadata": {"name": "qr-9", "creationTimestamp": "2026-09-03T10:00:00Z"},
                "spec": {"owner": "zoe", "requested": {"cpu": 12}, "reason": "old"},
                "status": {"state": "pending"}
            }]}),
        ),
    ])
    .await;
    let r = get(&format!("{s}/admin/requests", s = s.base), &s.admin_token).await;
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
        Route::new(
            "GET",
            &format!("{API}/requests"),
            json!({"items": [{
                "metadata": {"name": "req-1", "creationTimestamp": "2026-09-04T10:00:00Z"},
                "spec": {"owner": "acme", "kind": "access", "requestedBy": "meera", "reason": "r",
                         "access": {"team": "acme", "role": "admin"}},
                "status": {"state": "pending"}
            }]}),
        ),
        Route::new("GET", &format!("{API}/quotarequests"), json!({"items": []})),
    ])
    .await;
    let r = get(&format!("{s}/admin/requests?kind=access", s = s.base), &s.admin_token).await;
    let rows: Vec<Value> = r.json().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "req-1");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --test api_admin_requests`
Expected: FAIL — 404, `/admin/requests` is not mounted.

- [ ] **Step 3: Implement the union**

In `crates/workspaces/src/api/admin.rs`, extend `RequestFilter` and add the union below `list_all_quota_requests`:

```rust
#[derive(serde::Deserialize, Default)]
pub(crate) struct RequestFilter {
    owner: Option<String>,
    state: Option<crd::RequestState>,
    /// New in the generic queue; the legacy list ignores it (every legacy row is a quota row, so
    /// any other value simply drops them).
    kind: Option<crd::RequestKind>,
}
```

```rust
/// A legacy `QuotaRequest` wearing the generic doc — the migration has not necessarily run, and a
/// console must never have to know that. `requested` becomes the `quota` block, and everything
/// else it never had stays absent.
fn legacy_doc(r: &crd::QuotaRequest) -> super::RequestDoc {
    let st = r.status.clone().unwrap_or_default();
    super::RequestDoc {
        id: r.name_any(),
        owner: r.spec.owner.clone(),
        kind: crd::RequestKind::Quota,
        // The old CRD never recorded an author, and inventing one would be worse than an empty
        // string: the owner is who it was for, not necessarily who typed it.
        requested_by: String::new(),
        reason: r.spec.reason.clone(),
        quota: Some(r.spec.requested.clone()),
        access: None,
        region: None,
        other: None,
        state: st.state,
        decided_by: st.decided_by,
        decided_at: st.decided_at,
        note: st.note,
        resolution: None,
        created_at: r.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()),
    }
}

/// The whole queue, both CRDs, newest first. Filtering happens here (server-side of this process,
/// client-side of the k3s API) for the same reason `list_all_quota_requests_inner` does it: the
/// fleet-wide row count is small and neither CRD carries a label to select a kind or a state on.
pub(crate) async fn list_requests_inner(
    s: &ApiState,
    f: &RequestFilter,
) -> Result<Vec<super::RequestDoc>, Response> {
    let api: Api<crd::Request> = Api::all(kube(s)?.clone());
    let mut rows: Vec<super::RequestDoc> = api
        .list(&ListParams::default())
        .await
        .map_err(kube_err)?
        .items
        .iter()
        .map(super::generic_doc)
        .collect();
    if f.kind.is_none_or(|k| k == crd::RequestKind::Quota) {
        let legacy = RequestFilter { owner: f.owner.clone(), state: f.state, kind: None };
        rows.extend(list_all_quota_requests_inner(s, &legacy).await?.iter().map(legacy_doc));
    }
    rows.retain(|r| {
        f.owner.as_deref().is_none_or(|o| r.owner == o)
            && f.state.is_none_or(|st| r.state == st)
            && f.kind.is_none_or(|k| r.kind == k)
    });
    // `created_at` is an RFC 3339 string, so string order IS time order; an undated row (a
    // just-created object the API server has not stamped) sorts last rather than first.
    rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(rows)
}

async fn list_requests(State(s): State<Arc<ApiState>>, Query(f): Query<RequestFilter>) -> Result<Response, Response> {
    Ok(Json(list_requests_inner(&s, &f).await?).into_response())
}
```

`overview.rs` constructs `RequestFilter { owner: None, state: Some(..) }` — add `kind: None` there. Mount the route in `router()` next to the quota-request ones:

```rust
        .route("/admin/requests", get(list_requests))
```

`RequestDoc`'s fields must be `pub(crate)` (Task 2 declared them so) for `legacy_doc` to build one.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p kloudlite-git-workspaces --test api_admin_requests --test api_admin_overview`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/api/admin.rs crates/workspaces/src/api/admin/overview.rs crates/workspaces/tests/api_admin_requests.rs
git commit -m "Serve one admin request queue over both request CRDs"
```

---

### Task 4: Granting team access from the admin process

**Files:**
- Modify: `crates/workspaces/src/api/mod.rs` (the `Directory` trait, ~line 150)
- Modify: `bins/api/src/main.rs` (`impl Directory for Dir`)
- Test: `crates/workspaces/src/api/mod.rs` (`#[cfg(test)] mod tests`, the existing one)

**Interfaces:**
- Produces: `pub enum GrantAccess { Done, NoSuchUser, NoSuchTeam, Refused(String), Unsupported }` and `Directory::grant_access(&self, team: &str, user: &str, role: TeamRole) -> GrantAccess` with a default body returning `Unsupported`.
- Consumes (in `bins/api`): `kloudlite_git_pulls::directory::Directory::{add_member, set_role}`, `AddMember`, `Membership`, `Role`.

> **Resolution (stated in the plan header, repeated here because this task is where it bites):** the spec asks for "a peer route on the server tier". There is none to reuse, and none is needed: `bins/api` — the very process that decides requests — already holds the mongo `Directory` and wears the workspaces `Directory` trait (`bins/api/src/main.rs`'s `Dir`). A peer HTTP hop would be this process calling itself. One trait method is the whole grant. `crates/api`'s `set_role` handler stays exactly as it is for the interactive team page.

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` at the bottom of `crates/workspaces/src/api/mod.rs`:

```rust
    /// A directory that has not implemented granting answers `Unsupported`, and the approve arm
    /// turns that into a refusal — never a silent success on a membership nothing wrote.
    #[tokio::test]
    async fn a_directory_without_granting_refuses_rather_than_pretending() {
        struct Bare;
        #[async_trait::async_trait]
        impl super::Directory for Bare {
            async fn teams_for(&self, _u: &str) -> Vec<String> {
                Vec::new()
            }
            async fn is_live(&self, _j: &str) -> bool {
                false
            }
            async fn for_owner(&self, _o: &str) -> Option<super::OwnerMaterial> {
                None
            }
            async fn team_role(&self, _u: &str, _t: &str) -> Option<super::TeamRole> {
                None
            }
            async fn is_team(&self, _s: &str) -> bool {
                false
            }
        }
        assert_eq!(
            Bare.grant_access("acme", "meera", super::TeamRole::Admin).await,
            super::GrantAccess::Unsupported
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --lib a_directory_without_granting`
Expected: FAIL — `no method named grant_access`.

- [ ] **Step 3: Add the trait method**

In `crates/workspaces/src/api/mod.rs`, beside the `Directory` trait:

```rust
/// What a membership write did. Not a `Result`: "no such user" and "no such team" are answers a
/// decider needs to read back verbatim, not errors to log.
#[derive(Debug, PartialEq, Eq)]
pub enum GrantAccess {
    Done,
    NoSuchUser,
    NoSuchTeam,
    /// The directory's own refusal, already in words fit to show — a last-owner demotion, say.
    Refused(String),
    /// No directory is wired, or this one cannot write. A DEFAULT so every test stub in this crate
    /// keeps compiling; the approve arm turns it into a 503 rather than a false success.
    Unsupported,
}
```

and inside `pub trait Directory`:

```rust
    /// Put `user` into `team` at `role`, creating the membership if they are not in it yet. Only
    /// the admin process implements this — the user role has no route that could call it.
    async fn grant_access(&self, _team: &str, _user: &str, _role: TeamRole) -> GrantAccess {
        GrantAccess::Unsupported
    }
```

- [ ] **Step 4: Wire the mongo directory**

In `bins/api/src/main.rs`, inside `impl kloudlite_git_workspaces::api::Directory for Dir`:

```rust
    async fn grant_access(
        &self,
        team: &str,
        user: &str,
        role: kloudlite_git_workspaces::api::TeamRole,
    ) -> kloudlite_git_workspaces::api::GrantAccess {
        use kloudlite_git_pulls::directory::{AddMember, Membership, Role};
        use kloudlite_git_workspaces::api::{GrantAccess, TeamRole};
        let role = match role {
            TeamRole::Owner => Role::Owner,
            TeamRole::Admin => Role::Admin,
            TeamRole::Member => Role::Member,
        };
        // Add first, then fall through to a role change: a grant is "be in this team at this
        // role", and whether they were already in it is not the decider's problem. `add_member`'s
        // filter carries its own duplicate check, so this is safe to retry.
        match self.0.add_member(team, user, role).await {
            Ok(AddMember::Added) => return GrantAccess::Done,
            Ok(AddMember::NoSuchUser) => return GrantAccess::NoSuchUser,
            Ok(AddMember::NoSuchTeam) => return GrantAccess::NoSuchTeam,
            Ok(AddMember::AlreadyMember) => {}
            Err(e) => {
                tracing::error!(error = %e, %team, "granting team access");
                return GrantAccess::Refused("the directory could not be written".into());
            }
        }
        match self.0.set_role(team, user, role).await {
            Ok(Membership::Done) => GrantAccess::Done,
            Ok(Membership::NotAMember) => GrantAccess::NoSuchUser,
            Ok(Membership::NoSuchTeam) => GrantAccess::NoSuchTeam,
            Ok(Membership::LastOwner) => {
                GrantAccess::Refused("a team must keep at least one owner".into())
            }
            Err(e) => {
                tracing::error!(error = %e, %team, "setting team role");
                GrantAccess::Refused("the directory could not be written".into())
            }
        }
    }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p kloudlite-git-workspaces --lib a_directory_without_granting && cargo build -p kloudlite-git-api`
Expected: PASS, and the binary builds.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/api/mod.rs bins/api/src/main.rs
git commit -m "Let the admin process grant team membership through the directory"
```

---

### Task 5: `POST /admin/requests/{id}/approve|deny`

**Files:**
- Modify: `crates/workspaces/src/api/admin.rs`
- Test: `crates/workspaces/tests/api_admin_requests.rs`

**Interfaces:**
- Consumes: `crd::Request`, `crd::RequestStatus`, `crd::QuotaSpec.regions` (Task 1); `list_requests_inner` (Task 3); `Directory::grant_access`, `GrantAccess` (Task 4); existing `write_quota`, `overlay`, `quota::effective`, `scope::is_team`, `audit`, `audited`, `require_note`.
- Produces: `POST /admin/requests/{id}/approve`, `POST /admin/requests/{id}/deny` — both returning the decided `RequestDoc`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/workspaces/tests/api_admin_requests.rs`:

```rust
/// Quota approve is unchanged in substance: the Quota is written FIRST, then the request marked,
/// and the operator's edited values win over what was asked.
#[tokio::test]
async fn approving_a_quota_request_writes_the_quota_first() {
    let s = admin_server(vec![
        Route::new("GET", &format!("{API}/requests/req-1"), pending_quota_request()),
        Route::new("GET", &format!("{API}/quotas/karthik"), not_found()),
        Route::new("POST", &format!("{API}/quotas"), json!({"metadata": {"name": "karthik"}, "spec": {}})),
        Route::new("PUT", &format!("{API}/requests/req-1/status"), decided("req-1", "approved")),
    ])
    .await;
    let r = post(
        &format!("{s}/admin/requests/req-1/approve", s = s.base),
        &s.admin_token,
        json!({"note": "ok", "quota": {"workspaces": 12}}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let written = s.rec.body("POST", &format!("{API}/quotas")).unwrap();
    assert_eq!(written["spec"]["workspaces"], 12, "the operator's edit, not the asked-for 9");
}

/// Region approve records the grant on the owner's Quota and says so in `resolution` — the spec's
/// "a recorded decision only" has to be visible to the person who reads the decision back.
#[tokio::test]
async fn approving_a_region_request_records_the_grant() {
    let s = admin_server(vec![
        Route::new("GET", &format!("{API}/requests/req-2"), pending_region_request()),
        Route::new("GET", &format!("{API}/regions/westeurope-k3s"), active_region()),
        Route::new("GET", &format!("{API}/quotas/karthik"), quota_object(&[])),
        Route::new("PATCH", &format!("{API}/quotas/karthik"), quota_object(&["westeurope-k3s"])),
        Route::new("PUT", &format!("{API}/requests/req-2/status"), decided("req-2", "approved")),
    ])
    .await;
    let r = post(&format!("{s}/admin/requests/req-2/approve", s = s.base), &s.admin_token, json!({"note": "ok"}))
        .await;
    assert_eq!(r.status(), 200);
    let patched = s.rec.body("PATCH", &format!("{API}/quotas/karthik")).unwrap();
    assert_eq!(patched["spec"]["regions"], json!(["westeurope-k3s"]));
    let sent = s.rec.body("PUT", &format!("{API}/requests/req-2/status")).unwrap();
    assert!(
        sent["status"]["resolution"].as_str().unwrap().contains("recorded"),
        "the resolution has to say the grant is recorded, not enforced"
    );
}

/// An `other` request has nothing to write, so the free-text resolution IS the decision. Without
/// it, approve would mark a request done having done nothing at all.
#[tokio::test]
async fn approving_an_other_request_needs_a_resolution() {
    let s = admin_server(vec![Route::new("GET", &format!("{API}/requests/req-3"), pending_other_request())]).await;
    let r = post(&format!("{s}/admin/requests/req-3/approve", s = s.base), &s.admin_token, json!({"note": "ok"}))
        .await;
    assert_eq!(r.status(), 422);
    assert!(s.rec.body("PUT", &format!("{API}/requests/req-3/status")).is_none());
}

/// Two admins racing: the second sees the decision, not a silent overwrite.
#[tokio::test]
async fn an_already_decided_request_is_a_conflict() {
    let s = admin_server(vec![Route::new("GET", &format!("{API}/requests/req-4"), decided("req-4", "approved"))]).await;
    let r = post(&format!("{s}/admin/requests/req-4/deny", s = s.base), &s.admin_token, json!({"note": "no"})).await;
    assert_eq!(r.status(), 409);
}

/// Deny writes nothing but the mark, and the note is required — the asker has to be told why.
#[tokio::test]
async fn deny_requires_a_note() {
    let s = admin_server(vec![Route::new("GET", &format!("{API}/requests/req-5"), pending_quota_request())]).await;
    let r = post(&format!("{s}/admin/requests/req-5/deny", s = s.base), &s.admin_token, json!({})).await;
    assert_eq!(r.status(), 422);
}
```

Add the small fixture helpers beside them (`pending_quota_request`, `pending_region_request`, `pending_other_request`, `decided`, `quota_object`, `active_region`) as plain `fn () -> Value` builders in the same file, following the shapes in Task 1 and Task 3's `stored`:

```rust
fn pending_quota_request() -> Value {
    json!({"metadata": {"name": "req-1", "creationTimestamp": "2026-09-04T10:00:00Z"},
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces --test api_admin_requests`
Expected: FAIL — 404 on every decision path.

- [ ] **Step 3: Implement the decision routes**

In `crates/workspaces/src/api/admin.rs`, after `list_requests`:

```rust
#[derive(serde::Deserialize, Default)]
struct GenericDecision {
    #[serde(default)]
    note: Option<String>,
    /// A quota decision's edited grant, replacing `spec.quota` before `overlay` runs — approve
    /// grants what was actually submitted, which is the original ask unless edited.
    #[serde(default)]
    quota: Option<crd::RequestedQuota>,
    /// Required on an `other` approve and free on the rest: an `other` request has nothing to
    /// write, so this sentence IS the decision.
    #[serde(default)]
    resolution: Option<String>,
}

fn decision_body(body: &axum::body::Bytes) -> Result<GenericDecision, Response> {
    if body.is_empty() {
        return Ok(GenericDecision::default());
    }
    serde_json::from_slice(body).map_err(|_| (StatusCode::BAD_REQUEST, "invalid body").into_response())
}

async fn pending_generic(s: &ApiState, id: &str) -> Result<crd::Request, Response> {
    check_path_segment(id)?;
    let api: Api<crd::Request> = Api::all(kube(s)?.clone());
    let r = api.get_opt(id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    if !super::is_pending_generic(&r) {
        return Err((StatusCode::CONFLICT, "that request has already been decided").into_response());
    }
    Ok(r)
}

/// Stamp the outcome. `status`, not spec: the request is what was asked, the decision is what
/// happened to it, and only this tier ever writes it (no controller reconciles a request).
async fn decide_generic(
    s: &ApiState,
    id: &str,
    state: crd::RequestState,
    by: &str,
    note: Option<String>,
    resolution: Option<String>,
) -> Result<Response, Response> {
    let api: Api<crd::Request> = Api::all(kube(s)?.clone());
    let patch = serde_json::json!({"status": {
        "state": state, "decidedBy": by, "decidedAt": chrono::Utc::now().to_rfc3339(),
        "note": note, "resolution": resolution,
    }});
    let out = api.patch_status(id, &PatchParams::default(), &Patch::Merge(&patch)).await.map_err(kube_err)?;
    Ok(Json(super::generic_doc(&out)).into_response())
}

/// The consequential half of an approve, per kind. Returns the sentence that goes into
/// `status.resolution`: what this decision actually DID.
///
/// Every arm writes its effect BEFORE the request is marked, and the caller audits between the
/// two — if the mark then fails, the grant still landed and the row says so, which is the only
/// order that never claims something that did not happen.
async fn apply_approval(
    s: &ApiState,
    r: &crd::Request,
    d: &GenericDecision,
    actor: &str,
) -> Result<String, Response> {
    let owner = r.spec.owner.clone();
    match r.spec.kind {
        crd::RequestKind::Quota => {
            let want = d.quota.clone().or_else(|| r.spec.quota.clone()).unwrap_or_default();
            let client = kube(s)?;
            let api: Api<crd::Quota> = Api::all(client.clone());
            let base = match api.get_opt(&owner).await.map_err(kube_err)? {
                Some(q) => q.spec.clone(),
                None => {
                    let team = scope::is_team(s, &owner).await;
                    crate::quota::effective(client, &owner, team).await.map_err(kube_err)?
                }
            };
            audited(s, actor, "request.approved", &owner, d.note.clone(), write_quota(s, &owner, overlay(base, &want)).await)
                .await?;
            Ok("quota written".to_string())
        }
        crd::RequestKind::Access => {
            let ask = r.spec.access.clone().ok_or_else(|| {
                // `validate` runs on create, so this is only reachable for an object written
                // around `/v1` — a restored backup, say. Refuse rather than approve a no-op.
                (StatusCode::UNPROCESSABLE_ENTITY, "this access request carries no access block").into_response()
            })?;
            let dir = s.directory.as_ref().ok_or_else(|| {
                (StatusCode::SERVICE_UNAVAILABLE, "team lookup not configured on this node").into_response()
            })?;
            let role = match ask.role.as_str() {
                "owner" => super::TeamRole::Owner,
                "admin" => super::TeamRole::Admin,
                _ => super::TeamRole::Member,
            };
            // The grant is for the person who ASKED, not for `spec.owner`: an access request's
            // owner is the asker's own slug and the team is what they do not have yet.
            let who = r.spec.requested_by.clone();
            let outcome = dir.grant_access(&ask.team, &who, role).await;
            let msg = match outcome {
                super::GrantAccess::Done => format!("{who} is {} of {}", ask.role, ask.team),
                super::GrantAccess::NoSuchUser => {
                    return Err(audit_refusal(s, actor, &owner, d, StatusCode::UNPROCESSABLE_ENTITY, "no such user").await)
                }
                super::GrantAccess::NoSuchTeam => {
                    return Err(audit_refusal(s, actor, &owner, d, StatusCode::UNPROCESSABLE_ENTITY, "no such team").await)
                }
                super::GrantAccess::Refused(why) => {
                    return Err(audit_refusal(s, actor, &owner, d, StatusCode::CONFLICT, &why).await)
                }
                super::GrantAccess::Unsupported => {
                    return Err(audit_refusal(
                        s, actor, &owner, d, StatusCode::SERVICE_UNAVAILABLE,
                        "this process cannot write team membership",
                    )
                    .await)
                }
            };
            Ok(msg)
        }
        crd::RequestKind::Region => {
            let ask = r.spec.region.clone().ok_or_else(|| {
                (StatusCode::UNPROCESSABLE_ENTITY, "this region request carries no region block").into_response()
            })?;
            // The region must still exist and be active — an approve is a decision somebody will
            // read months from now, and one naming a retired region is worse than a refusal.
            super::check_region(s, &ask.region).await?;
            let client = kube(s)?;
            let api: Api<crd::Quota> = Api::all(client.clone());
            let mut base = match api.get_opt(&owner).await.map_err(kube_err)? {
                Some(q) => q.spec.clone(),
                None => {
                    let team = scope::is_team(s, &owner).await;
                    crate::quota::effective(client, &owner, team).await.map_err(kube_err)?
                }
            };
            if !base.regions.contains(&ask.region) {
                base.regions.push(ask.region.clone());
            }
            audited(s, actor, "request.approved", &owner, d.note.clone(), write_quota(s, &owner, base).await).await?;
            Ok(format!(
                "{} recorded as a granted region for {owner}; placement does not read it yet",
                ask.region
            ))
        }
        crd::RequestKind::Other => {
            let text = d.resolution.as_deref().unwrap_or("").trim().to_string();
            if text.is_empty() {
                return Err((StatusCode::UNPROCESSABLE_ENTITY, "resolution is required").into_response());
            }
            Ok(text)
        }
    }
}

/// A refusal from a grant is a decision that did NOT happen, and 409-and-up refusals are evidence
/// (same rule as `audited`) — recorded here because the failure is the directory's answer, not a
/// `Result` `audited` could wrap.
async fn audit_refusal(
    s: &ApiState,
    actor: &str,
    owner: &str,
    d: &GenericDecision,
    code: StatusCode,
    why: &str,
) -> Response {
    audit(s, actor, "request.approved", owner, d.note.clone(), format!("error:{}", code.as_u16())).await;
    (code, why.to_string()).into_response()
}

async fn approve_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let d = decision_body(&body)?;
    let r = audited(&s, &c.name, "request.approved", &id, d.note.clone(), pending_generic(&s, &id).await).await?;
    let resolution = apply_approval(&s, &r, &d, &c.name).await?;
    // The effect above is the consequential write; `decide_generic` only marks the request, and if
    // IT fails the grant still landed — so the row is recorded here rather than after the second
    // fallible call.
    audit(&s, &c.name, "request.approved", &r.spec.owner, d.note.clone(), "ok").await;
    decide_generic(&s, &id, crd::RequestState::Approved, &c.name, d.note, Some(resolution)).await
}

/// Deny: mark the request only, no grant of any kind. The note is required — the asker reads it.
async fn deny_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let d = decision_body(&body)?;
    let note = require_note(d.note.as_deref().unwrap_or(""))?;
    let r = audited(&s, &c.name, "request.denied", &id, Some(note.clone()), pending_generic(&s, &id).await).await?;
    let out = audited(
        &s,
        &c.name,
        "request.denied",
        &r.spec.owner,
        Some(note.clone()),
        decide_generic(&s, &id, crd::RequestState::Denied, &c.name, Some(note.clone()), None).await,
    )
    .await?;
    audit(&s, &c.name, "request.denied", &r.spec.owner, Some(note), "ok").await;
    Ok(out)
}
```

Mount both, next to `/admin/requests`:

```rust
        .route("/admin/requests/{id}/approve", post(approve_request))
        .route("/admin/requests/{id}/deny", post(deny_request))
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces --test api_admin_requests`
Expected: PASS (7 tests, including Task 3's two)

- [ ] **Step 5: Check the whole crate and the lint gate**

Run: `cargo test -p kloudlite-git-workspaces && cargo clippy --workspace -- -D warnings`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/api/admin.rs crates/workspaces/tests/api_admin_requests.rs
git commit -m "Decide generic requests with kind-specific approve semantics"
```

---

### Task 6: One-shot migration of the legacy queue

**Files:**
- Modify: `crates/workspaces/src/api/admin.rs`
- Test: `crates/workspaces/tests/api_admin_requests.rs`

**Interfaces:**
- Produces: `POST /admin/requests/migrate` → `{"copied": n, "skipped": n}`.

- [ ] **Step 1: Write the failing test**

```rust
/// Idempotent by uid: the new object's NAME is derived from the legacy object's uid, so a second
/// run finds it already there and copies nothing. An operator has to be able to run this twice.
#[tokio::test]
async fn the_migration_is_idempotent_by_uid() {
    let legacy = json!({"items": [{
        "metadata": {"name": "qr-9", "uid": "7f1c1a2e-0000-4000-8000-000000000001",
                     "creationTimestamp": "2026-09-03T10:00:00Z"},
        "spec": {"owner": "zoe", "requested": {"cpu": 12}, "reason": "old"},
        "status": {"state": "pending"}
    }]});
    let s = admin_server(vec![
        Route::new("GET", &format!("{API}/quotarequests"), legacy),
        // The API server answers a second create of the same name with 409 AlreadyExists — the
        // migration's own idempotence, not a check it does itself.
        Route::conflict("POST", &format!("{API}/requests")),
    ])
    .await;
    let r = post(&format!("{s}/admin/requests/migrate", s = s.base), &s.admin_token, json!({"note": "migrate"}))
        .await;
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["copied"], 0);
    assert_eq!(body["skipped"], 1);
    let sent = s.rec.body("POST", &format!("{API}/requests")).unwrap();
    assert_eq!(sent["metadata"]["name"], "q-7f1c1a2e-0000-4000-8000-000000000001");
    assert_eq!(sent["spec"]["kind"], "quota");
    assert_eq!(sent["spec"]["quota"]["cpu"], 12);
}
```

If `Route::conflict` does not exist in `kube_test.rs`, use whatever the harness offers for a non-2xx canned reply (`Route::status("POST", path, 409)`); check the file and match its spelling.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --test api_admin_requests the_migration_is_idempotent`
Expected: FAIL — 404, the route does not exist.

- [ ] **Step 3: Implement it**

```rust
/// Copy every legacy `QuotaRequest` into a `Request` of kind quota, once. Idempotent because the
/// new object's NAME is derived from the old one's uid: a re-run collides on create and skips,
/// so nothing has to remember whether this already ran.
///
/// A route rather than a `kl` subcommand or a boot step: it needs the admin process's cluster
/// credentials, it must be auditable, and it must be a decision an operator TAKES rather than a
/// thing that happens to their cluster on a rolling restart. The legacy CRD is retired in a later
/// release; until then the queue unions both anyway, so running this late costs nothing.
async fn migrate_requests(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NoteBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let note = require_note(&body.note)?;
    let legacy = list_all_quota_requests_inner(&s, &RequestFilter::default()).await?;
    let api: Api<crd::Request> = Api::all(kube(&s)?.clone());
    let (mut copied, mut skipped) = (0u32, 0u32);
    for old in &legacy {
        let Some(uid) = old.metadata.uid.as_deref() else {
            // No uid means the object never reached etcd — nothing to copy, and no stable name
            // to make the copy idempotent under.
            skipped += 1;
            continue;
        };
        let mut r = crd::Request::new(
            &format!("q-{uid}"),
            crd::RequestSpec {
                owner: old.spec.owner.clone(),
                kind: crd::RequestKind::Quota,
                // The old CRD never recorded an author; the owner is who it was for.
                requested_by: old.spec.owner.clone(),
                reason: old.spec.reason.clone(),
                quota: Some(old.spec.requested.clone()),
                access: None,
                region: None,
                other: None,
            },
        );
        r.metadata.labels =
            Some(std::collections::BTreeMap::from([(OWNER_LABEL.to_string(), old.spec.owner.clone())]));
        match api.create(&PostParams::default(), &r).await {
            Ok(_) => copied += 1,
            // Already migrated. The one error this loop swallows on purpose — it is the idempotence.
            Err(kube::Error::Api(e)) if e.code == 409 => skipped += 1,
            Err(e) => return Err(kube_err(e)),
        }
    }
    // The copies already landed; the row goes in before the response is built.
    audit(&s, &c.name, "migrate-requests", "quotarequests", Some(note), "ok").await;
    Ok(Json(serde_json::json!({"copied": copied, "skipped": skipped})).into_response())
}
```

Mount it: `.route("/admin/requests/migrate", post(migrate_requests))` — **before** `/admin/requests/{id}/approve` is not required (the paths differ in their last segment), but keep it adjacent for readability. Add `OWNER_LABEL` to the module's `use super::*` reach if it is not already visible (it is re-exported at `api/mod.rs:608`).

> Note: a migrated copy carries no `status`, so it reads as pending. A legacy request that was already DECIDED must not reappear in the pending queue — carry the decision over too by patching status right after a successful create, using `decide_generic(&s, &format!("q-{uid}"), st.state, st.decided_by.as_deref().unwrap_or(""), st.note.clone(), None)` for any `old.status` whose state is not `Pending`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces --test api_admin_requests`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/api/admin.rs crates/workspaces/tests/api_admin_requests.rs
git commit -m "Copy legacy quota requests into the generic queue once"
```

---

### Task 7: Web — the request client, the four forms, and "My requests"

**Files:**
- Create: `web/apps/web/src/lib/requests.ts`, `web/apps/web/src/lib/requests.test.ts`
- Modify: `web/apps/web/src/lib/api.ts:1023-1075`
- Create: `web/apps/web/src/app/(shell)/requests/page.tsx`, `web/apps/web/src/app/(shell)/requests/actions.ts`
- Create: `web/apps/web/src/components/app/new-request-dialog.tsx`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/quota-actions.ts`, `web/apps/web/src/components/app/user-menu.tsx`

**Interfaces:**
- Consumes: `RequestDoc` as Task 2 serializes it.
- Produces: `RequestKind`, `KINDS`, `kindLabel(k)`, `type RequestDoc` (in `lib/requests.ts`); `createRequest`, `listRequests`, `adminListRequests`, `adminDecideRequest` (in `lib/api.ts`); `newRequest(prev, formData)` server action.
- **Sub-project C builds the superadmin console pages. This task adds the admin CLIENT functions and nothing under `app/(shell)/superadmin/`.**

- [ ] **Step 1: Write the failing test**

Create `web/apps/web/src/lib/requests.test.ts`:

```ts
import { expect, test } from "bun:test";
import { KINDS, kindLabel, blockFor } from "./requests";

test("every kind has a label", () => {
  expect(KINDS.map(kindLabel)).toEqual(["More quota", "Team access", "A region", "Something else"]);
});

/** The api refuses a request whose block does not match its kind (422), so the form must send
 *  exactly one — this is the function that decides which. */
test("a form builds exactly the block for its kind", () => {
  const form = new FormData();
  form.set("team", "acme");
  form.set("role", "admin");
  expect(blockFor("access", form)).toEqual({ access: { team: "acme", role: "admin" } });

  const q = new FormData();
  q.set("workspaces", "12");
  q.set("cpu", "");
  expect(blockFor("quota", q)).toEqual({ quota: { workspaces: 12 } });
});

/** An empty required field is a refusal here, not a 422 from the api after a round trip. */
test("an incomplete form is refused before it is sent", () => {
  expect(() => blockFor("region", new FormData())).toThrow("Pick a region.");
  expect(() => blockFor("quota", new FormData())).toThrow("Raise at least one dimension.");
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd web && bun test apps/web/src/lib/requests.test.ts`
Expected: FAIL — cannot resolve `./requests`.

- [ ] **Step 3: Write the client module**

Create `web/apps/web/src/lib/requests.ts`:

```ts
import { DIMS, type QuotaDim } from "@/lib/quota";

/** The four kinds, in the order the picker offers them: most common first. The words are the
 *  api's own `spec.kind` values, so a doc's kind is directly a key here — one vocabulary. */
export const KINDS = ["quota", "access", "region", "other"] as const;
export type RequestKind = (typeof KINDS)[number];

export function kindLabel(k: RequestKind): string {
  return { quota: "More quota", access: "Team access", region: "A region", other: "Something else" }[k];
}

export type RequestDoc = {
  id: string;
  owner: string;
  kind: RequestKind;
  requestedBy: string;
  reason: string;
  quota?: Partial<Record<QuotaDim, number>>;
  access?: { team: string; role: string };
  region?: { region: string };
  other?: { title: string; body: string };
  state: "pending" | "approved" | "denied";
  decidedBy?: string | null;
  decidedAt?: string | null;
  note?: string | null;
  resolution?: string | null;
  createdAt?: string | null;
};

/** Exactly one block, matching the kind — the api 422s anything else, so the form decides here
 *  rather than discovering it after a round trip. Throws a sentence fit to show. */
export function blockFor(kind: RequestKind, form: FormData): Record<string, unknown> {
  const str = (k: string) => String(form.get(k) ?? "").trim();
  if (kind === "quota") {
    const quota: Partial<Record<QuotaDim, number>> = {};
    for (const d of DIMS) {
      const raw = str(d);
      if (!raw) continue;
      const n = Number(raw);
      if (!Number.isFinite(n) || n < 0) throw new Error(`That is not a valid amount for ${d}.`);
      quota[d] = n;
    }
    if (Object.keys(quota).length === 0) throw new Error("Raise at least one dimension.");
    return { quota };
  }
  if (kind === "access") {
    const team = str("team");
    const role = str("role");
    if (!team) throw new Error("Name the team.");
    if (!["member", "admin", "owner"].includes(role)) throw new Error("Pick a role.");
    return { access: { team, role } };
  }
  if (kind === "region") {
    const region = str("region");
    if (!region) throw new Error("Pick a region.");
    return { region: { region } };
  }
  const title = str("title");
  const body = str("body");
  if (!title || !body) throw new Error("A title and a description, please.");
  return { other: { title, body } };
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd web && bun test apps/web/src/lib/requests.test.ts`
Expected: PASS (3 tests)

- [ ] **Step 5: Add the api functions**

In `web/apps/web/src/lib/api.ts`, after the existing quota-request block, and re-export the type so callers have one import site:

```ts
export type { RequestDoc, RequestKind } from "@/lib/requests";
import type { RequestDoc } from "@/lib/requests";

/** `requestedBy` is set by the api from the session, never sent — a request that could name its
 *  own author is not evidence of who asked. */
export function createRequest(
  body: { owner?: string; kind: string; reason: string } & Record<string, unknown>,
  token: string,
) {
  return call<RequestDoc>("/v1/requests", { method: "POST", token, body: JSON.stringify(body) });
}

/** The caller's own and their teams'; `owner` narrows to one they may act on. */
export function listRequests(owner: string | undefined, token: string) {
  const q = owner ? `?owner=${encodeURIComponent(owner)}` : "";
  return call<RequestDoc[]>(`/v1/requests${q}`, { method: "GET", token });
}

/** The whole queue, every owner and every kind, unioned over the new CRD and the legacy one.
 *  `kind`/`owner`/`state` are server-side narrowing; anything finer stays client-side. */
export function adminListRequests(token: string, filter?: { kind?: string; owner?: string; state?: string }) {
  const q = new URLSearchParams();
  if (filter?.kind) q.set("kind", filter.kind);
  if (filter?.owner) q.set("owner", filter.owner);
  if (filter?.state) q.set("state", filter.state);
  const qs = q.toString();
  return adminCall<RequestDoc[]>(`/admin/requests${qs ? `?${qs}` : ""}`, { method: "GET", token });
}

/** `quota` is the operator's edited grant on a quota decision; `resolution` is REQUIRED on an
 *  `other` approve (there is nothing else for that approve to do) and optional elsewhere. `note`
 *  is required on every deny. */
export function adminDecideRequest(
  id: string,
  decision: "approve" | "deny",
  body: { note?: string; quota?: Record<string, number>; resolution?: string },
  token: string,
) {
  return adminCall<RequestDoc>(`/admin/requests/${encodeURIComponent(id)}/${decision}`, {
    method: "POST",
    token,
    body: JSON.stringify(body),
  });
}
```

Leave `QuotaRequestDoc`, `createQuotaRequest`, `listQuotaRequests`, `adminListQuotaRequests` and `adminDecideQuotaRequest` in place: the existing superadmin pages still call them, and sub-project C is what retires them.

- [ ] **Step 6: The server action and the dialog**

Create `web/apps/web/src/app/(shell)/requests/actions.ts`:

```ts
"use server";

import { tokenOr } from "@/lib/api-token";
import * as api from "@/lib/api";
import { blockFor, KINDS, type RequestKind } from "@/lib/requests";

export type NewRequestState = { ok?: true; error?: string } | null;

export async function newRequest(_prev: NewRequestState, formData: FormData): Promise<NewRequestState> {
  const kind = String(formData.get("kind") ?? "") as RequestKind;
  if (!(KINDS as readonly string[]).includes(kind)) return { error: "Pick what you are asking for." };
  const owner = String(formData.get("owner") ?? "").trim();
  const reason = String(formData.get("reason") ?? "").trim();
  if (!reason) return { error: "Say what this is for." };

  let block: Record<string, unknown>;
  try {
    block = blockFor(kind, formData);
  } catch (e) {
    return { error: e instanceof Error ? e.message : "That form is not complete." };
  }

  const token = await tokenOr();
  if (typeof token !== "string") return { error: token.error };

  const r = await api.createRequest({ owner: owner || undefined, kind, reason, ...block }, token);
  if (!r.ok) {
    if (r.kind === "conflict") return { error: "A request of this kind is already pending." };
    if (r.kind === "forbidden") return { error: "Only a team admin can ask on a team's behalf." };
    return { error: r.message || "Could not send the request." };
  }
  return { ok: true };
}
```

Create `web/apps/web/src/components/app/new-request-dialog.tsx` by copying `quota-request-dialog.tsx` and changing three things, so the shape stays the sibling it is: a `<select name="kind">` over `KINDS` driving a `useState` that picks which field group renders; the quota group is the existing six-input grid verbatim; access is `team` (text) + `role` (select over member/admin/owner); region is a `<select name="region">` over the `regions` prop; other is `title` (Input) + `body` (Textarea). Reason stays required for every kind. The action is `newRequest`.

In `web/apps/web/src/components/app/user-menu.tsx`, above the Superadmin item:

```tsx
        <DropdownMenuItem asChild>
          <Link href="/requests"><Inbox className="size-4" /> My requests</Link>
        </DropdownMenuItem>
```

with `Inbox` added to the `lucide-react` import. The "New request" entry lives on `/requests` as the page's one primary action rather than as a dropdown item that would have to carry dialog state into the shell.

Create `web/apps/web/src/app/(shell)/requests/page.tsx` following `repo-list.tsx`'s list shape: `listRequests(undefined, token)` server-side, a row per request showing kind, the one-line ask (from the block that is set), state, and `note`/`resolution` once decided; the `NewRequestDialog` in the header; an empty state of one sentence plus that action.

- [ ] **Step 7: Point the 409 dialog at the new route**

In `web/apps/web/src/app/(shell)/[owner]/(org)/quota-actions.ts`, swap the api call, leaving the rest of `requestQuota` exactly as it is:

```ts
  // Same dialog, same 409-driven entry point — a kind-quota Request now, so a quota ask and every
  // other kind share one queue and one decision path.
  const r = await api.createRequest({ owner: owner || undefined, kind: "quota", reason, quota: requested }, token);
```

- [ ] **Step 8: Verify the web**

Run: `cd web && bun run typecheck && bun run lint && bun test`
Expected: PASS. If the editor disagrees, trust `bunx tsc --noEmit -p apps/web/tsconfig.json`.

- [ ] **Step 9: Commit**

```bash
git add web/apps/web/src/lib/requests.ts web/apps/web/src/lib/requests.test.ts web/apps/web/src/lib/api.ts \
  "web/apps/web/src/app/(shell)/requests" web/apps/web/src/components/app/new-request-dialog.tsx \
  web/apps/web/src/components/app/user-menu.tsx \
  "web/apps/web/src/app/(shell)/[owner]/(org)/quota-actions.ts"
git commit -m "Raise any kind of request from the web and list your own"
```

---

### Task 8: Docs and the end-to-end assertion

**Files:**
- Modify: `CLAUDE.md` (the "Allocation is bounded by a `Quota` per owner" paragraph)
- Modify: `deploy/k3s/README.md`
- Modify: `tests/ws_e2e.sh` (the quota-request block, ~lines 459-487, and the final `echo "OK: ..."` line)

**Interfaces:** none — documentation and the e2e gate.

- [ ] **Step 1: Update CLAUDE.md**

Replace the last sentence of that paragraph ("A raise is a `QuotaRequest` CR: …") with:

```markdown
A raise, or anything else somebody has to be granted, is a `Request` CR (`crd::Request`, kinds
quota / access / region / other): the owner, or a team member whose directory role is at least
admin, opens ONE pending request per owner PER KIND, and only a superadmin decides it. Approve is
kind-specific and always writes its effect BEFORE marking the request — quota writes the `Quota`
through the one writer `write_quota`, access sets team membership through the directory the admin
process already holds (`Directory::grant_access`, no peer hop: `bins/api` IS the tier with the
directory), region appends to `Quota.spec.regions` and is a RECORDED grant only until per-owner
region gating exists, and `other` requires a free-text resolution. `QuotaRequest` is the retired
predecessor: still readable, unioned into `GET /admin/requests`, copied over once by
`POST /admin/requests/migrate`, and deleted as a CRD in a later release.
```

- [ ] **Step 2: Add the release note**

In `deploy/k3s/README.md`, in the per-release apply list:

```markdown
- **Generic requests (2026-09-04):** `kubectl apply -f deploy/k3s/crds.yaml` (adds `Request`) and
  `kubectl apply -f deploy/k3s/api-rbac.yaml` (adds `requests` to both ClusterRoles). Apply both
  BEFORE rolling the api image: without the CRD every `/v1/requests` create 404s from the API
  server, and without the RBAC the admin process 403s on the queue. Then, once per cluster,
  `POST /admin/requests/migrate` with a note — idempotent, safe to repeat.
```

- [ ] **Step 3: Extend the e2e**

In `tests/ws_e2e.sh`, after the existing "checking the approval raised the limit" block, add:

```bash
log "opening an access request as the user"
ACC_JSON=$(curl -fsS -X POST "$BASE/v1/requests" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind":"access","reason":"e2e","access":{"team":"'"$E2E_TEAM"'","role":"member"}}')
ACC_ID=$(echo "$ACC_JSON" | field id)
[ -n "$ACC_ID" ] || fail "no id in access request response: $ACC_JSON"

log "checking a second pending access request is refused while the quota one is not"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/requests" \
  -H "Authorization: Bearer $USER_TOKEN" -H 'Content-Type: application/json' \
  -d '{"kind":"access","reason":"again","access":{"team":"'"$E2E_TEAM"'","role":"admin"}}')
[ "$CODE" = "409" ] || fail "a second pending access request must 409, got $CODE"

log "checking the queue unions both request CRDs"
curl -fsS "$ADMIN_BASE/admin/requests" -H "Authorization: Bearer $ADMIN_TOKEN" \
  | grep -q "\"id\":\"$ACC_ID\"" || fail "the access request is missing from the admin queue"

log "approving the access request as a superadmin"
curl -fsS -X POST "$ADMIN_BASE/admin/requests/$ACC_ID/approve" -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"note":"e2e"}' >/dev/null || fail "access approve failed"

log "checking the approval actually set the membership"
curl -fsS "$BASE/v1/teams/$E2E_TEAM" -H "Authorization: Bearer $USER_TOKEN" \
  | grep -q "$USER_NAME" || fail "the approved access request did not add the member"

log "checking the decided request cannot be decided twice"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$ADMIN_BASE/admin/requests/$ACC_ID/deny" \
  -H "Authorization: Bearer $ADMIN_TOKEN" -H 'Content-Type: application/json' -d '{"note":"late"}')
[ "$CODE" = "409" ] || fail "a second decision must 409, got $CODE"
```

`$E2E_TEAM` must be a team the script creates and that `$USER_NAME` is NOT already in — check whether the script already makes one (`grep -n 'v1/teams' tests/ws_e2e.sh`); if it does not, create one as a second user before this block, using the same `POST /v1/teams` shape the api takes, and remember the script's own convention for a second identity.

Append to the final `echo "OK: ..."` summary, inside the quota clause: `, generic requests (access request opened, one-pending-per-kind, unioned queue, approve sets membership, second decision 409s)`.

- [ ] **Step 4: Verify what can be verified here**

Run: `cargo test && cargo clippy --workspace -- -D warnings && bash -n tests/ws_e2e.sh`
Expected: PASS. `./tests/ws_e2e.sh` itself needs a Linux VM with btrfs and k3s — it exits 77 on this Mac, which is a SKIP, never a pass. Say so plainly rather than claiming the e2e ran.

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md deploy/k3s/README.md tests/ws_e2e.sh
git commit -m "Document generic requests and assert an access grant end to end"
```

---

## Self-Review

**1. Spec coverage (§B).** CRD shape with the four kinds and per-kind blocks → Task 1. Status with `resolution` → Task 1. `requestedBy` from the signed-in user → Task 2. One pending per owner per kind, 409 → Task 2 (test + implementation). Team-admin rule for a team owner → Task 2 (reuses `may_request_for`). Approve: quota → Task 5 (editable values kept); access → Tasks 4+5; region → Task 5 (`Quota.spec.regions`, "recorded decision only" stated in `resolution`); other → Task 5 (required resolution). Every decision audits and emits `request.approved`/`request.denied` → Task 5. The 409 dialog keeps opening a quota request → Task 7 Step 7. "New request" picker + four forms, "My requests" → Task 7. Legacy objects stay readable, unioned, migrated once, retired later → Tasks 3, 6, 8. The console pages themselves are §C and are explicitly out of scope.

**2. Placeholder scan.** Every code step carries the real code. Three steps describe a UI composition rather than a full file (Task 7 Step 6's dialog and page): each names the exact sibling to copy, the exact field names, and the exact prop; the api-facing contract they must satisfy is fully specified in Task 2 and Task 7 Steps 3 and 5. Task 8 Step 3 and Task 6 Step 3's `Route::conflict` both instruct a grep for the local spelling rather than asserting one — deliberate, because those two names could not be verified without running the tools this plan is written without.

**3. Type consistency.** `RequestDoc`/`generic_doc`/`requests_of_generic`/`is_pending_generic`/`create_request_inner` (Task 2) are consumed under exactly those names in Tasks 3, 5 and 6. `RequestFilter` grows `kind` in Task 3 and the `overview.rs` construction is fixed in the same step. `GrantAccess`/`grant_access`/`TeamRole` (Task 4) are consumed under those names in Task 5. `QuotaSpec.regions` (Task 1) is read and written in Task 5 and asserted in Task 1's own test. The web's `RequestDoc` (Task 7) mirrors Task 2's camelCase serialization field for field.

## Resolved ambiguities

1. **Region grants do not go on `OwnerBinding.status.regions`.** An `OwnerBinding` is one object per `{owner, region}` and is authored by the claiming agent, so a per-owner list of granted regions has no coherent home on it, and an agent-written status would race the admin's write. The grant is recorded as `Quota.spec.regions` — the one per-owner, cluster-scoped, admin-written object — which needs no new RBAC (`kloudlite-git-admin` already patches `quotas`) and is exactly where per-owner region gating will read it.
2. **No new peer route on the server tier for access grants.** `bins/api` already holds the mongo directory behind the workspaces `Directory` trait, so a peer hop would be the admin process calling itself. One trait method (`grant_access`, default `Unsupported`) plus its implementation in `bins/api/src/main.rs` is the whole grant; `crates/api`'s `set_role` handler is untouched.
3. **An access request's `spec.owner` is the ASKER, not the team.** The team is what they do not have yet, so it cannot also be the owner that authorizes the ask (`may_request_for` would refuse a non-member outright). `access.team` names the team, `requestedBy` is who gets the role, and the per-kind pending rule counts against the asker's own slug.
