# Quotas, quota requests and superadmin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every owner (person or team) a per-owner allocation ceiling enforced by `/v1` and by Kubernetes, a request/approve path for raising it, and a real superadmin claim to replace the email-allowlist env var.

**Architecture:** Two new cluster-scoped CRDs — `Quota` (name = owner slug, plus two `default-*` fallbacks) and `QuotaRequest` — written only by `/v1`. Usage is computed on every request by listing the owner's already-label-indexed `Workspace`/`Environment`/`Volume`/`Snapshot` objects; nothing is cached and no counter is stored. `/v1` refuses over-quota allocation with a 409 sentence naming the dimension; the agent additionally writes a Kubernetes `ResourceQuota` into each owner namespace as the platform-side cap on cpu/memory. Superadmin becomes a boolean claim in the session JWT, minted at sign-in from a `superadmins` collection in the mongo directory (bootstrapped from `RUSTIC_GIT_WORKSPACES_ADMINS`), read by `/v1` and by the web's new `/admin` area.

**Tech Stack:** Rust (`kube`/`k8s-openapi`, `axum`, `serde`, `jsonwebtoken`, `mongodb`), Kubernetes CRDs + RBAC, Next.js app router + `bun:test`.

**Spec:** `docs/superpowers/specs/2026-09-03-quotas-and-superadmin-design.md` — binding. Read it before Task 1; every 409 sentence, every default number and every rule below is copied from it.

## Global Constraints

These apply to every task; they are not repeated per task.

- **No tool attribution anywhere.** Commit subjects are imperative sentence case, no `Co-Authored-By`, no `Generated with`, no Claude reference in code comments or docs.
- **Comments say WHY, never what.** Match the density of `bins/server/src/router/route.rs`. Do not narrate the code.
- **Keep every `// ponytail:` marker** you edit near, and add one for any deliberate shortcut with a named ceiling and upgrade path.
- **Vocabulary:** *workspace*, *environment*, *push*, *snapshot*, *working copy*. Never "fork", never "commit" for a snapshot in user-facing text, never "job" or "queue".
- **Never authorize on a label.** `spec.owner` is the truth; `rustic-git.io/owner` is a listing view only. Every new listing may select on the label, but every decision re-reads `spec.owner`.
- **Superadmin is a claim, not an owner.** It never changes who owns anything, never appears in a `spec.owner`, and never widens a quota by itself.
- **Usage is computed from the CRDs on every request, never cached and never stored in a status field.**
- **Detached volumes count** toward `diskGb`; deleting snapshots is how disk is returned.
- **A team's quota is the team's.** Objects whose `spec.owner` is the team slug count against the team only, never against the member who created them.
- **Quotas block new allocation only.** Nothing is ever auto-deleted, auto-stopped or auto-approved.
- **Bootstrap defaults, verbatim from the spec:**

  | | person (`default-user`) | team (`default-team`) |
  |---|---|---|
  | workspaces | 5 | 20 |
  | environments | 2 | 8 |
  | snapshots | 20 | 80 |
  | diskGb | 100 | 400 |
  | cpu | 8 | 32 |
  | memoryGb | 32 | 128 |

- **The 409 sentence shape, verbatim:** `"{dimension}: {used} of {limit} in use; request more under Quota"` — e.g. `workspaces: 5 of 5 in use; request more under Quota`. Dimension words are exactly `workspaces`, `environments`, `snapshots`, `diskGb`, `cpu`, `memoryGb`.
- **Gates — run all of these before every commit, unpiped:**

  ```bash
  cargo test -p rustic-git-workspaces -p rustic-git-agent-bin -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```

  Plus, in the task that changes a CRD: `CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml` then re-run the test without `CRD_REGEN` and commit `deploy/k3s/crds.yaml` in the same commit. Plus, in any web task, from `web/`: `bun run lint`, `bunx tsc --noEmit -p apps/web/tsconfig.json`, `bun test` (all three, unpiped, `echo exit=$?` after each).

  Note on clippy: `--all-targets` has pre-existing lints in test targets. The bar is **no new warnings in files you touch**; if the run is red, read the paths and confirm none is a file this task edited.

## Ordering note: this plan runs AFTER the review-fix plans

Two things the review-fix plans (`docs/superpowers/plans/2026-09-03-review-server-deploy.md` and its siblings) do first, which change where you type:

1. **`crates/workspaces/src/api.rs` may already be a module directory.** If `crates/workspaces/src/api.rs` still exists as one file, all `/v1` edits below go in it. If it is now `crates/workspaces/src/api/` with submodules, then: the new quota routes and handlers go in a new `crates/workspaces/src/api/quota.rs` declared from `api/mod.rs`, `Caller`/`caller`/`may_act_on`/`require_admin`/`Directory` stay in `api/mod.rs` (they are shared), and each enforcement call goes in whichever submodule holds that handler. **The handler and helper function names below are unchanged either way** — locate by name (`rg -n "async fn create_ws" crates/workspaces/src`), not by line number.
2. **The per-owner creation cap (review item C2) is a stopgap this plan replaces.** Those plans add a fixed per-owner cap on `create_ws`/`create_env` as a holding measure. Task 3 deletes it: `rg -n "C2|creation cap|MAX_WORKSPACES|per-owner cap" crates/workspaces/src` and remove the constant, the check and its tests in the same commit that lands `quota::check`. The `Quota` object is the cap now, and two caps that can disagree is the bug.

Also: the review plans make `trait Directory`'s methods **required** (no default bodies). Task 4 adds `team_role` as a required method, so every stub implementing `Directory` must gain it. The stubs, at the time of writing: `crates/workspaces/tests/api_teams.rs` (`StubMembership`), `crates/workspaces/tests/api_user.rs` (four), and one in-module `Stub` inside `api.rs`. Let the compiler enumerate them.

## File Structure

| File | Create/Modify | Responsibility |
|---|---|---|
| `crates/workspaces/src/crd.rs` | Modify | `Quota`/`QuotaRequest` types, the two default names, the bootstrap numbers, `all_crds()` |
| `crates/workspaces/src/quota.rs` | Create | Usage computation, effective-limit lookup, the 409 sentence, quantity parsing |
| `crates/workspaces/src/api.rs` (or `api/`) | Modify | `Caller`, `require_admin`, `may_act_on`, `Directory::team_role`, `GET /v1/quota`, the quota-request routes, every enforcement call |
| `crates/workspaces/src/k8s.rs` | Modify | `resource_quota()` builder |
| `crates/workspaces/src/lib.rs` | Modify | `pub mod quota;` |
| `crates/core/src/jwt.rs` | Modify | `Claims.superadmin`, `mint_admin` |
| `crates/pulls/src/directory/mod.rs` | Modify | `superadmins` collection, `SuperAdmin` row, the four methods |
| `crates/api/src/teams.rs` | Modify | Mint the claim at sign-in and at handle claim; `/api/admin/superadmins/{user}` |
| `crates/api/src/lib.rs` | Modify | Mount the two superadmin routes |
| `bins/api/src/main.rs` | Modify | `Dir::team_role`, superadmin bootstrap at boot, drop the `admins` set |
| `bins/agent/src/binding.rs` | Modify | `ResourceQuota` per workspace namespace |
| `bins/agent/src/controller/environment.rs` | Modify | `ResourceQuota` per environment namespace |
| `deploy/k3s/crds.yaml` | Regenerate | Generated artifact |
| `deploy/k3s/api-rbac.yaml`, `deploy/k3s/agent-rbac.yaml` | Modify | New verbs |
| `deploy/k3s/README.md`, `CLAUDE.md` | Modify | Release note with apply order; the invariant paragraph |
| `web/apps/web/src/lib/quota.ts` (+ `.test.ts`) | Create | Pure usage-bar math and labels |
| `web/apps/web/src/lib/api.ts` | Modify | Five typed calls |
| `web/apps/web/src/components/app/quota-bar.tsx`, `quota-request-dialog.tsx` | Create | Usage bar, request form |
| `web/apps/web/src/app/(shell)/[owner]/(org)/page.tsx` | Modify | Show the bar |
| `web/apps/web/src/app/(shell)/admin/*` | Create | The admin area |
| `web/apps/web/src/components/app/shell-nav.tsx` | Modify | `admin` is a root page |
| `tests/ws_e2e.sh` | Modify | Limit → request → approve → succeed |

---

### Task 1: The `Quota` and `QuotaRequest` CRDs

**Files:**
- Modify: `crates/workspaces/src/crd.rs` (append the new types near `OwnerBindingSpec`, and edit `all_crds()`)
- Modify: `crates/workspaces/tests/crd_yaml.rs` (`every_crd_has_a_status_subresource_and_the_right_node_selector`'s match)
- Modify: `deploy/k3s/crds.yaml` (regenerated, never hand-edited)
- Modify: `deploy/k3s/api-rbac.yaml`, `deploy/k3s/agent-rbac.yaml`
- Test: `crates/workspaces/tests/crd_yaml.rs`

**Interfaces:**
- Produces: `crd::Quota` / `crd::QuotaSpec { workspaces: u32, environments: u32, snapshots: u32, disk_gb: u64, cpu: u32, memory_gb: u32 }` / `crd::QuotaStatus`; `crd::QuotaRequest` / `crd::QuotaRequestSpec { owner: String, requested: crd::RequestedQuota, reason: String }` / `crd::QuotaRequestStatus { state: crd::RequestState, decided_by: Option<String>, decided_at: Option<String>, note: Option<String> }`; `crd::RequestedQuota` (all six fields `Option`); `crd::RequestState::{Pending, Approved, Denied}`; `crd::DEFAULT_USER_QUOTA`, `crd::DEFAULT_TEAM_QUOTA` (`&str` names), `crd::default_quota(team: bool) -> QuotaSpec`.
- Consumes: nothing.

- [ ] **Step 1: Write the failing test**

Append to `crates/workspaces/tests/crd_yaml.rs`:

```rust
/// Both new kinds ship, and the request's state enum is published as an `enum` rather than a free
/// string: a typo in `state` must be a 422 from the API server, not a request that is neither
/// pending nor decided and so is invisible to both the queue and the one-pending check.
#[test]
fn quota_kinds_are_published() {
    let kinds: Vec<String> = all_crds().into_iter().map(|c| c.spec.names.kind).collect();
    assert!(kinds.iter().any(|k| k == "Quota"), "{kinds:?}");
    assert!(kinds.iter().any(|k| k == "QuotaRequest"), "{kinds:?}");

    let crd = all_crds().into_iter().find(|c| c.spec.names.kind == "QuotaRequest").unwrap();
    let schema = crd.spec.versions[0].schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
    let status = schema.properties.as_ref().unwrap()["status"].properties.as_ref().unwrap();
    let states = status["state"].enum_.as_ref().expect("state must be an enum");
    let words: Vec<String> = states.iter().map(|v| v.0.trim_matches('"').to_string()).collect();
    assert_eq!(words, vec!["pending", "approved", "denied"], "{words:?}");
}

/// The bootstrap numbers are the spec's table, and they are what an owner with no `Quota` object
/// of their own gets — so a change here is a change to what every unlisted owner may allocate.
#[test]
fn the_bootstrap_defaults_are_the_specs_table() {
    let u = rustic_git_workspaces::crd::default_quota(false);
    assert_eq!((u.workspaces, u.environments, u.snapshots, u.disk_gb, u.cpu, u.memory_gb), (5, 2, 20, 100, 8, 32));
    let t = rustic_git_workspaces::crd::default_quota(true);
    assert_eq!((t.workspaces, t.environments, t.snapshots, t.disk_gb, t.cpu, t.memory_gb), (20, 8, 80, 400, 32, 128));
}
```

In the same file, extend the existing `every_crd_has_a_status_subresource_and_the_right_node_selector` match with the two new arms (they are placed by nobody, so no selectable fields):

```rust
            "Quota" | "QuotaRequest" => &[],
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test crd_yaml; echo exit=$?`
Expected: FAIL — `default_quota` and the two kinds do not exist (compile error), and once they do, `generated_crds_match_the_committed_manifest` fails until the manifest is regenerated.

- [ ] **Step 3: Write the implementation**

In `crates/workspaces/src/crd.rs`, after `OwnerBindingStatus`:

```rust
/// What ONE owner — a person or a team slug — may allocate. Cluster-scoped, named by the owner
/// slug, written only by a superadmin through `/v1`.
///
/// Two `default-*` objects are the fallback for an owner with no object of their own, because a
/// slug does not say which it is: `/v1` knows (a team slug is one the directory answers for) and
/// picks. Nothing here is a count of what EXISTS — usage is computed from the objects themselves
/// on every request (`quota::usage`), so no field of this object can drift from the truth.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
    version = "v1alpha1",
    kind = "Quota",
    plural = "quotas",
    shortname = "qta",
    status = "QuotaStatus",
    printcolumn = r#"{"name":"Workspaces","type":"integer","jsonPath":".spec.workspaces"}"#,
    printcolumn = r#"{"name":"Environments","type":"integer","jsonPath":".spec.environments"}"#,
    printcolumn = r#"{"name":"DiskGb","type":"integer","jsonPath":".spec.diskGb"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSpec {
    /// Live working copies of kind Workspace.
    pub workspaces: u32,
    /// Live working copies of kind Environment.
    pub environments: u32,
    /// Snapshots — pushes, not sync points. The agent's own transient cuts are its business and
    /// are never anyone's allocation.
    pub snapshots: u32,
    /// Sum of `Volume.spec.quotaGb` over every volume of this owner, DETACHED INCLUDED: disk kept
    /// by snapshots after a working copy is deleted is still the owner's disk.
    pub disk_gb: u64,
    /// Whole cores, summed over live working copies' limits.
    pub cpu: u32,
    pub memory_gb: u32,
}

/// Nothing writes this today. It exists because every CRD in this repo has a status subresource —
/// without one a status write folds into spec and the RBAC spec/status split becomes decorative —
/// and `crd_yaml.rs` enforces that for every kind.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

pub const DEFAULT_USER_QUOTA: &str = "default-user";
pub const DEFAULT_TEAM_QUOTA: &str = "default-team";

/// The bootstrap table from the design doc, owner-approved 2026-09-03. In code rather than in a
/// manifest so an owner with no `Quota` and a cluster with no `default-*` object still has a
/// definite ceiling — a missing fallback object must not mean "unlimited".
pub fn default_quota(team: bool) -> QuotaSpec {
    if team {
        QuotaSpec { workspaces: 20, environments: 8, snapshots: 80, disk_gb: 400, cpu: 32, memory_gb: 128 }
    } else {
        QuotaSpec { workspaces: 5, environments: 2, snapshots: 20, disk_gb: 100, cpu: 8, memory_gb: 32 }
    }
}

/// The six fields again, every one optional: a request raises the dimensions it names and says
/// nothing about the rest, so approving it must not silently reset a limit somebody already
/// granted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestedQuota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environments: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_gb: Option<u32>,
}

/// A person asking for more, and the decision on it.
///
/// The one kind whose STATUS `/v1` writes rather than a controller: no controller reconciles a
/// request — a person decides it — so the decision has nowhere else to live. Requests are never
/// deleted by the system; the record of who asked for what, and who said yes, is the point.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
    version = "v1alpha1",
    kind = "QuotaRequest",
    plural = "quotarequests",
    shortname = "qreq",
    status = "QuotaRequestStatus",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"State","type":"string","jsonPath":".status.state"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct QuotaRequestSpec {
    pub owner: String,
    pub requested: RequestedQuota,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaRequestStatus {
    pub state: RequestState,
    /// The deciding superadmin's email, for the audit trail. Never an owner of anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// An enum, not a string, so the API server refuses a typo with a 422 — the same reason `Phase` is
/// one. A request with no status at all is pending: `/v1` creates the object and patches status
/// separately, and the window between the two must not read as "decided".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RequestState {
    #[default]
    Pending,
    Approved,
    Denied,
}
```

Extend `all_crds()`:

```rust
pub fn all_crds() -> Vec<CustomResourceDefinition> {
    vec![
        Volume::crd(),
        Workspace::crd(),
        Environment::crd(),
        OwnerBinding::crd(),
        Snapshot::crd(),
        VolumeReplica::crd(),
        Quota::crd(),
        QuotaRequest::crd(),
    ]
}
```

- [ ] **Step 4: Regenerate the manifest and run the tests**

```bash
CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml; echo exit=$?
cargo test -p rustic-git-workspaces --test crd_yaml; echo exit=$?
```
Expected: PASS on the second run, with `deploy/k3s/crds.yaml` modified.

- [ ] **Step 5: RBAC for the two kinds**

In `deploy/k3s/api-rbac.yaml`, inside the `rustic-git-api` ClusterRole rules, after the `volumes` rule:

```yaml
  # Quotas: read on every path (usage answers and enforcement both need the owner's effective
  # limit), and write ONLY on the approve path — `create` because approving an owner who has never
  # had an object of their own writes their first one, `patch` because approving one who has raises
  # the dimensions the request named and must leave the rest alone. No `delete`: removing an
  # owner's ceiling would silently promote them to the default, which is a different number.
  - apiGroups: ["rustic-git.io"]
    resources: ["quotas"]
    verbs: ["get", "list", "create", "patch"]
  # QuotaRequests are the one kind whose STATUS this tier writes: no controller reconciles a
  # request — a superadmin decides it — so the decision has nowhere else to live. Requests are
  # never deleted, so no `delete` here either; the record of who asked is the point.
  - apiGroups: ["rustic-git.io"]
    resources: ["quotarequests"]
    verbs: ["get", "list", "create"]
  - apiGroups: ["rustic-git.io"]
    resources: ["quotarequests/status"]
    verbs: ["patch", "update"]
```

In `deploy/k3s/agent-rbac.yaml`, add to the header table (under the `ownerbindings` block):

```
#   quotas (rustic-git.io)                 get,list,watch               binding/environment reconcile:
#                                                                       the owner's effective ceiling,
#                                                                       written into the namespace as
#                                                                       a ResourceQuota. READ ONLY —
#                                                                       a quota is desired state and
#                                                                       the agent never writes one
#   resourcequotas                         create,patch                 ensure (server-side apply)
```

and the rules:

```yaml
  # Read-only, and deliberately so: a `Quota` is DESIRED state, written by a superadmin through
  # /v1. The agent only projects it into the namespace as a `ResourceQuota`, which is why
  # agent-admission.yaml gains nothing for this feature — there is no agent spec write to refuse.
  - apiGroups: ["rustic-git.io"]
    resources: ["quotas"]
    verbs: ["get", "list", "watch"]
  # The platform-side ceiling on cpu/memory, applied like every other namespaced object the agent
  # materializes. Never deleted: it must not vanish when a binding is rewritten.
  - apiGroups: [""]
    resources: ["resourcequotas"]
    verbs: ["create", "patch"]
```

- [ ] **Step 6: Run the gates**

```bash
cargo test -p rustic-git-workspaces -p rustic-git-agent-bin -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
```
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/crd.rs crates/workspaces/tests/crd_yaml.rs deploy/k3s/crds.yaml deploy/k3s/api-rbac.yaml deploy/k3s/agent-rbac.yaml
git commit -m "Add the Quota and QuotaRequest custom resources"
```

---

### Task 2: Usage computation and `GET /v1/quota`

**Files:**
- Create: `crates/workspaces/src/quota.rs`
- Modify: `crates/workspaces/src/lib.rs` (add `pub mod quota;` beside the other module declarations)
- Modify: `crates/workspaces/src/api.rs` (router + one handler)
- Test: unit tests inside `crates/workspaces/src/quota.rs`; recorder test in a new `crates/workspaces/tests/api_quota.rs`

**Interfaces:**
- Consumes: `crd::QuotaSpec`, `crd::default_quota`, `crd::DEFAULT_USER_QUOTA`, `crd::DEFAULT_TEAM_QUOTA` (Task 1); `api::may_act_on`, `api::caller`, `api::kube`, `api::kube_err` (existing).
- Produces:
  - `quota::Usage { workspaces: u32, environments: u32, snapshots: u32, disk_gb: u64, cpu: u32, memory_gb: u32 }` (`Default`, `Serialize` camelCase)
  - `quota::usage(c: &kube::Client, owner: &str) -> Result<Usage, kube::Error>`
  - `quota::effective(c: &kube::Client, owner: &str, team: bool) -> Result<crd::QuotaSpec, kube::Error>`
  - `quota::Dim` (`Copy`), `quota::Dim::word(self) -> &'static str`
  - `quota::refuse(dim: Dim, used: u64, limit: u64) -> String`
  - `quota::millicores(q: &str) -> u64`, `quota::mebibytes(q: &str) -> u64`
  - `GET /v1/quota?owner=<slug>` answering `{"owner":…,"limit":QuotaSpec,"used":Usage}`

- [ ] **Step 1: Write the failing unit tests**

Create `crates/workspaces/src/quota.rs` containing ONLY this test module for now (the code follows in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities_parse_the_forms_this_repo_writes() {
        // `PodResources::default` and `k8s::env_unit_resources` between them write exactly these.
        assert_eq!(millicores("4"), 4000);
        assert_eq!(millicores("250m"), 250);
        assert_eq!(millicores("2"), 2000);
        assert_eq!(mebibytes("8Gi"), 8192);
        assert_eq!(mebibytes("2730Mi"), 2730);
        assert_eq!(mebibytes("4Gi"), 4096);
        // An unparseable quantity is 0, never a panic and never a silent huge number: a bad value
        // must not be a way to look over quota, and it must not take the whole listing down.
        assert_eq!(millicores("nonsense"), 0);
        assert_eq!(mebibytes(""), 0);
    }

    #[test]
    fn the_refusal_names_the_dimension_the_limit_and_the_use() {
        assert_eq!(refuse(Dim::Workspaces, 5, 5), "workspaces: 5 of 5 in use; request more under Quota");
        assert_eq!(refuse(Dim::DiskGb, 96, 100), "diskGb: 96 of 100 in use; request more under Quota");
        assert_eq!(refuse(Dim::MemoryGb, 32, 32), "memoryGb: 32 of 32 in use; request more under Quota");
    }

    #[test]
    fn a_check_refuses_only_when_the_addition_would_cross_the_limit() {
        let limit = crate::crd::default_quota(false);
        let used = Usage { workspaces: 4, ..Default::default() };
        assert!(check(Dim::Workspaces, &limit, &used, 1).is_ok(), "4 + 1 of 5 fits");
        let used = Usage { workspaces: 5, ..Default::default() };
        let msg = check(Dim::Workspaces, &limit, &used, 1).unwrap_err();
        assert_eq!(msg, "workspaces: 5 of 5 in use; request more under Quota");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rustic-git-workspaces quota:: ; echo exit=$?`
Expected: FAIL — `crates/workspaces/src/quota.rs` is not a module yet (add `pub mod quota;` to `lib.rs` first, then it fails to compile on the missing items).

- [ ] **Step 3: Write the implementation**

Above the test module in `crates/workspaces/src/quota.rs`:

```rust
//! What an owner is using, computed from the objects themselves.
//!
//! Never cached, never stored in a status field. A stored counter can only be wrong in one
//! direction that matters — under-counting, which hands out allocation nobody has — and the lists
//! below are already indexed by the owner label, so the truth costs four list calls. The label is
//! the INDEX; every sum re-reads `spec.owner`, because a label is a view and never authorization.

use crate::crd;
use crate::k8s::OWNER_LABEL;
use kube::api::{Api, ListParams};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub workspaces: u32,
    pub environments: u32,
    pub snapshots: u32,
    pub disk_gb: u64,
    pub cpu: u32,
    pub memory_gb: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dim {
    Workspaces,
    Environments,
    Snapshots,
    DiskGb,
    Cpu,
    MemoryGb,
}

impl Dim {
    /// The word the 409 says. It is also the `QuotaSpec` field name on the wire, so the web can
    /// key the request form off the refusal without a second mapping to keep in step.
    pub fn word(self) -> &'static str {
        match self {
            Dim::Workspaces => "workspaces",
            Dim::Environments => "environments",
            Dim::Snapshots => "snapshots",
            Dim::DiskGb => "diskGb",
            Dim::Cpu => "cpu",
            Dim::MemoryGb => "memoryGb",
        }
    }

    fn of(self, q: &crd::QuotaSpec) -> u64 {
        match self {
            Dim::Workspaces => q.workspaces as u64,
            Dim::Environments => q.environments as u64,
            Dim::Snapshots => q.snapshots as u64,
            Dim::DiskGb => q.disk_gb,
            Dim::Cpu => q.cpu as u64,
            Dim::MemoryGb => q.memory_gb as u64,
        }
    }

    fn used(self, u: &Usage) -> u64 {
        match self {
            Dim::Workspaces => u.workspaces as u64,
            Dim::Environments => u.environments as u64,
            Dim::Snapshots => u.snapshots as u64,
            Dim::DiskGb => u.disk_gb,
            Dim::Cpu => u.cpu as u64,
            Dim::MemoryGb => u.memory_gb as u64,
        }
    }
}

/// The exact sentence the design doc specifies. One function, because the web keys off its shape
/// and six call sites formatting their own would drift.
pub fn refuse(dim: Dim, used: u64, limit: u64) -> String {
    format!("{}: {used} of {limit} in use; request more under Quota", dim.word())
}

/// Read-then-write, so two concurrent creates can overshoot by one. Accepted deliberately (design
/// doc §2): the `ResourceQuota` the agent writes is the hard stop for the dimensions where an
/// overshoot costs real capacity, and a lock across the API tier's replicas would cost more than
/// the one object it saves.
pub fn check(dim: Dim, limit: &crd::QuotaSpec, used: &Usage, adding: u64) -> Result<(), String> {
    let (have, cap) = (dim.used(used), dim.of(limit));
    if have + adding > cap {
        return Err(refuse(dim, have, cap));
    }
    Ok(())
}

fn owned_by(owner: &str) -> ListParams {
    ListParams::default().labels(&format!("{OWNER_LABEL}={owner}"))
}

/// A live working copy: the person's desired state is Running. A stopped workspace still holds its
/// disk (counted under `diskGb`) but is not holding cpu or memory on any node, and charging for
/// capacity nobody is occupying is what would make stopping pointless.
fn live(d: crd::DesiredState) -> bool {
    d == crd::DesiredState::Running
}

/// Milli-cores from a Kubernetes cpu quantity. `0` for anything unrecognised: a hand-edited spec
/// must not be a way to look under quota by making the sum unreadable, and it must not panic on a
/// listing every page does.
pub fn millicores(q: &str) -> u64 {
    match q.strip_suffix('m') {
        Some(n) => n.parse().unwrap_or(0),
        None => q.parse::<f64>().map(|v| (v * 1000.0) as u64).unwrap_or(0),
    }
}

/// Mebibytes from a Kubernetes memory quantity, for the two suffixes this repo writes.
pub fn mebibytes(q: &str) -> u64 {
    for (suffix, mib) in [("Gi", 1024u64), ("Mi", 1), ("G", 954), ("M", 1)] {
        if let Some(n) = q.strip_suffix(suffix) {
            return n.parse::<u64>().unwrap_or(0) * mib;
        }
    }
    // Bare bytes.
    q.parse::<u64>().unwrap_or(0) / (1024 * 1024)
}

fn ceil_div(n: u64, d: u64) -> u64 {
    n.div_ceil(d)
}

/// Everything `owner` is using right now. Four list calls, all label-selected.
pub async fn usage(c: &kube::Client, owner: &str) -> Result<Usage, kube::Error> {
    let ws: Api<crd::Workspace> = Api::all(c.clone());
    let envs: Api<crd::Environment> = Api::all(c.clone());
    let vols: Api<crd::Volume> = Api::all(c.clone());
    let snaps: Api<crd::Snapshot> = Api::all(c.clone());
    let lp = owned_by(owner);

    let (mut millis, mut mib) = (0u64, 0u64);
    let mut u = Usage::default();

    for w in ws.list(&lp).await?.items {
        if w.spec.owner != owner {
            continue;
        }
        u.workspaces += 1;
        if live(w.spec.desired_state) {
            millis += millicores(&w.spec.resources.cpu_limit);
            mib += mebibytes(&w.spec.resources.memory_limit);
        }
    }
    for e in envs.list(&lp).await?.items {
        if e.spec.owner != owner {
            continue;
        }
        u.environments += 1;
        if live(e.spec.desired_state) {
            // Every service gets the env unit — one definition, in `k8s::env_unit_resources`, used
            // by the StatefulSet and by the namespace's LimitRange. Reading it here is what keeps
            // the accounting and what actually runs from being two numbers.
            let unit = crate::k8s::env_unit_resources();
            let n = e.spec.services.len() as u64;
            millis += n * millicores(&unit.cpu_limit);
            mib += n * mebibytes(&unit.memory_limit);
        }
    }
    for v in vols.list(&lp).await?.items {
        if v.spec.owner != owner {
            continue;
        }
        // Detached volumes included: disk kept by snapshots after a working copy is deleted is
        // still the owner's disk, and deleting the snapshots is how they get it back.
        u.disk_gb += v.spec.quota_gb;
    }
    for s in snaps.list(&lp).await?.items {
        // `is_snapshot`, not `!spec.transient`: a legacy baseline is a sync point by shape rather
        // than by flag, and the agent's own sync points are never anyone's allocation.
        if s.spec.owner == owner && s.is_snapshot() {
            u.snapshots += 1;
        }
    }
    u.cpu = ceil_div(millis, 1000) as u32;
    u.memory_gb = ceil_div(mib, 1024) as u32;
    Ok(u)
}

/// The owner's own `Quota`, or the default object for their kind, or the compiled-in table.
///
/// Three levels because each missing level is a real state: a new owner has no object, a fresh
/// cluster has no `default-*` object either, and neither may read as "unlimited".
pub async fn effective(c: &kube::Client, owner: &str, team: bool) -> Result<crd::QuotaSpec, kube::Error> {
    let api: Api<crd::Quota> = Api::all(c.clone());
    if let Some(q) = api.get_opt(owner).await? {
        return Ok(q.spec);
    }
    let fallback = if team { crd::DEFAULT_TEAM_QUOTA } else { crd::DEFAULT_USER_QUOTA };
    if let Some(q) = api.get_opt(fallback).await? {
        return Ok(q.spec);
    }
    Ok(crd::default_quota(team))
}
```

Add `pub mod quota;` to `crates/workspaces/src/lib.rs`.

- [ ] **Step 4: Run the unit tests**

Run: `cargo test -p rustic-git-workspaces quota:: ; echo exit=$?`
Expected: PASS.

- [ ] **Step 5: Write the failing route test**

Create `crates/workspaces/tests/api_quota.rs`. Copy the `Server`/`server()`/`token()` harness from `crates/workspaces/tests/api_teams.rs:26-83` verbatim (same `MemStore`, same region registration, same `mock_client`) — do not invent a new fixture — then add:

```rust
//! `GET /v1/quota` and the quota-request routes, against a mocked API server
//! (`rustic_git_workspaces::kube_test`) with a stub `Directory` for team membership and roles.

const API: &str = "/apis/rustic-git.io/v1alpha1";

fn list_of(kind: &str, items: Vec<Value>) -> Value {
    json!({"apiVersion": "rustic-git.io/v1alpha1", "kind": format!("{kind}List"), "metadata": {}, "items": items})
}

fn ws_obj(id: &str, owner: &str, state: &str) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": id, "labels": {"rustic-git.io/owner": owner}},
        "spec": {"owner": owner, "team": "", "name": id, "region": "centralindia",
                 "image": "img:1", "desiredState": state, "packages": [],
                 "resources": {"cpuRequest": "2", "cpuLimit": "4", "memoryRequest": "4Gi", "memoryLimit": "8Gi"},
                 "storage": {"quotaGb": 20}}
    })
}

fn vol_obj(name: &str, owner: &str, gb: u64) -> Value {
    json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {"owner": owner, "team": "", "nodeName": "node-a", "region": "centralindia", "quotaGb": gb}
    })
}

/// The four listings usage sums, with no `Quota` object anywhere: the compiled-in default table is
/// what an owner with nothing of their own gets, and a cluster with no `default-user` object must
/// not read as unlimited.
#[tokio::test]
async fn quota_reports_the_default_limits_and_the_computed_use() {
    let routes = vec![
        get(format!("{API}/workspaces"), list_of("Workspace", vec![ws_obj("ws-1", "karthik", "running"), ws_obj("ws-2", "karthik", "stopped")])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![vol_obj("ws-1", "karthik", 20), vol_obj("ws-2", "karthik", 30)])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
    ];
    let s = server(true, routes).await;
    let doc: Value = reqwest::Client::new()
        .get(format!("{}/v1/quota", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send().await.unwrap()
        .json().await.unwrap();

    assert_eq!(doc["limit"]["workspaces"], 5, "{doc}");
    assert_eq!(doc["used"]["workspaces"], 2, "a stopped workspace still holds its place");
    // Detached and attached volumes alike.
    assert_eq!(doc["used"]["diskGb"], 50);
    // Only the RUNNING one occupies capacity: 4 cores, 8Gi.
    assert_eq!(doc["used"]["cpu"], 4);
    assert_eq!(doc["used"]["memoryGb"], 8);
}

/// A team's numbers are the team's. The caller is a member, so the read is allowed and the
/// fallback is the TEAM default, not their personal one.
#[tokio::test]
async fn a_member_reads_their_teams_quota_against_the_team_default() {
    let routes = vec![
        get(format!("{API}/workspaces"), list_of("Workspace", vec![])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        not_found(format!("{API}/quotas/acme")),
        not_found(format!("{API}/quotas/default-team")),
    ];
    let s = server(true, routes).await;
    let doc: Value = reqwest::Client::new()
        .get(format!("{}/v1/quota?owner=acme", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(doc["limit"]["workspaces"], 20, "{doc}");
}

/// An owner the caller is neither nor belongs to is a 404, the same answer every other
/// owner-scoped route gives: whether that owner exists is not theirs to learn.
#[tokio::test]
async fn another_owners_quota_is_not_readable() {
    let s = server(true, vec![]).await;
    let code = reqwest::Client::new()
        .get(format!("{}/v1/quota?owner=someoneelse", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send().await.unwrap()
        .status();
    assert_eq!(code, 404);
}
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo test -p rustic-git-workspaces --test api_quota -- --test-threads=1; echo exit=$?`
Expected: FAIL — no `/v1/quota` route, 404 on every case.

- [ ] **Step 7: Add the route and handler**

In `crates/workspaces/src/api.rs`'s `router()`, beside the other `get` routes:

```rust
        .route("/v1/quota", get(get_quota))
```

And the handler, in a `// ── quota ───` section near the volumes section:

```rust
#[derive(serde::Deserialize)]
struct QuotaQuery {
    /// Absent means the caller's own. A team slug they belong to is allowed; anything else is a
    /// 404, same as every other owner-scoped read.
    #[serde(default)]
    owner: Option<String>,
}

/// `GET /v1/quota?owner=` — the ceiling and what is against it, both for one owner.
///
/// Usage is computed here and nowhere else, on every request (see `quota::usage`'s module doc).
async fn get_quota(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<QuotaQuery>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let owner = q.owner.unwrap_or_else(|| c.name.clone());
    if !may_act_on(&s, &c, &owner).await {
        return Err(not_found());
    }
    let client = kube(&s)?;
    let team = owner != c.name;
    let limit = crate::quota::effective(client, &owner, team).await.map_err(kube_err)?;
    let used = crate::quota::usage(client, &owner).await.map_err(kube_err)?;
    Ok(Json(serde_json::json!({"owner": owner, "limit": limit, "used": used})).into_response())
}
```

Note `c.name` and `may_act_on(&s, &c, …)`: this task is written against the `Caller` type Task 5b introduces. **If Task 5b has not landed yet**, `caller()` still returns a `String` — write `let owner = q.owner.unwrap_or_else(|| c.clone());` and `may_act_on(&s, &c, &owner)`, and Task 5b's compiler sweep will convert this site with the rest.

- [ ] **Step 8: Run the tests**

Run: `cargo test -p rustic-git-workspaces --test api_quota -- --test-threads=1; echo exit=$?`
Expected: PASS.

- [ ] **Step 9: Run the gates and commit**

```bash
cargo test -p rustic-git-workspaces -p rustic-git-agent-bin -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
git add crates/workspaces/src/quota.rs crates/workspaces/src/lib.rs crates/workspaces/src/api.rs crates/workspaces/tests/api_quota.rs
git commit -m "Compute quota usage from the custom resources and serve it at /v1/quota"
```

---

### Task 3: Enforcement in `/v1`, and the stopgap cap deleted

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `create_ws`, `create_env`, `restore_ws`, `restore_env`, `clone_ws`, `clone_env`, `push_ws`, `push_env` (locate by name)
- Modify: `crates/workspaces/src/api.rs` — delete the review plans' per-owner creation cap (C2)
- Test: `crates/workspaces/tests/api_quota.rs`

**Interfaces:**
- Consumes: `quota::{usage, effective, check, Dim, Usage}`, `crd::QuotaSpec` (Task 2).
- Produces: `api::guard_alloc(s, owner, team, want: &[(Dim, u64)]) -> Result<(), Response>` — the ONE gate every allocating handler calls. Later tasks (and any future resize route) call this, never `quota::check` directly.

- [ ] **Step 1: Write the failing tests**

Append to `crates/workspaces/tests/api_quota.rs`:

```rust
/// The exact sentence from the design doc, on the exact status the web branches on. The routes
/// below all share `guard_alloc`, so one shape check per KIND of allocation is enough; what each
/// case pins is the DIMENSION the handler asks about.
#[tokio::test]
async fn a_create_at_the_workspace_limit_is_refused_with_the_specs_sentence() {
    let mut items = vec![];
    for i in 0..5 {
        items.push(ws_obj(&format!("ws-{i}"), "karthik", "stopped"));
    }
    let routes = vec![
        get(format!("{API}/workspaces"), list_of("Workspace", items)),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
    ];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "six", "region": "centralindia", "quota_gb": 5}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text().await.unwrap(), "workspaces: 5 of 5 in use; request more under Quota");
    // Nothing was written: the refusal happens before the object, or an over-quota create leaves a
    // workspace behind that the person was told they could not have.
    assert!(!s.rec.calls().iter().any(|c| c == &format!("POST {API}/workspaces")), "{:?}", s.rec.calls());
}

/// Disk is its own dimension and it counts DETACHED volumes: 96 of 100 GB used leaves no room for
/// a 5 GB workspace even though the workspace COUNT is fine.
#[tokio::test]
async fn a_create_that_would_cross_the_disk_limit_is_refused_on_disk() {
    let routes = vec![
        get(format!("{API}/workspaces"), list_of("Workspace", vec![])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![vol_obj("gone-1", "karthik", 96)])),
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
    ];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "big", "region": "centralindia", "quota_gb": 5}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text().await.unwrap(), "diskGb: 96 of 100 in use; request more under Quota");
}

/// A push at the snapshot limit is refused, and the working copy keeps running — the refusal is
/// before the `Snapshot` CR, so there is nothing half-cut to clean up.
#[tokio::test]
async fn a_push_at_the_snapshot_limit_is_refused_and_cuts_nothing() {
    let snaps: Vec<Value> = (0..20)
        .map(|i| json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": format!("snap-{i}"), "labels": {"rustic-git.io/owner": "karthik"}},
            "spec": {"volume": "ws-1", "owner": "karthik", "transient": false},
            "status": {"phase": "ready"}
        }))
        .collect();
    let mut ws = ws_obj("ws-1", "karthik", "running");
    ws["status"] = json!({"phase": "ready", "nodeName": "node-a", "volumeRef": "ws-1"});
    let routes = vec![
        get(format!("{API}/workspaces/ws-1"), ws),
        get(format!("{API}/workspaces"), list_of("Workspace", vec![])),
        get(format!("{API}/environments"), list_of("Environment", vec![])),
        get(format!("{API}/volumes"), list_of("Volume", vec![])),
        get(format!("{API}/snapshots"), list_of("Snapshot", snaps)),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
    ];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text().await.unwrap(), "snapshots: 20 of 20 in use; request more under Quota");
    assert!(!s.rec.calls().iter().any(|c| c == &format!("POST {API}/snapshots")), "{:?}", s.rec.calls());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p rustic-git-workspaces --test api_quota -- --test-threads=1; echo exit=$?`
Expected: FAIL — creates and pushes currently succeed (202) or 404 on unmocked routes.

- [ ] **Step 3: Add the gate**

In `crates/workspaces/src/api.rs`, in the quota section:

```rust
/// The ONE place `/v1` refuses an allocation.
///
/// Every route that brings a new working copy, a new disk or a new snapshot into existence goes
/// through here, so the sentence, the status and the read-then-write window are decided once. Two
/// list calls per allocation; nothing is cached, deliberately (`quota::usage`).
///
/// The `owner` is the OBJECT's owner, never the caller: a team's working copies count against the
/// team and nobody else. A superadmin gets no exemption — the claim says who may act, never how
/// much may exist.
async fn guard_alloc(
    s: &ApiState,
    owner: &str,
    team: bool,
    want: &[(crate::quota::Dim, u64)],
) -> Result<(), Response> {
    let c = kube(s)?;
    let limit = crate::quota::effective(c, owner, team).await.map_err(kube_err)?;
    let used = crate::quota::usage(c, owner).await.map_err(kube_err)?;
    for (dim, adding) in want {
        if let Err(msg) = crate::quota::check(*dim, &limit, &used, *adding) {
            return Err((StatusCode::CONFLICT, msg).into_response());
        }
    }
    Ok(())
}

/// What a new workspace costs, from the values the handler has already resolved and clamped.
fn workspace_cost(quota_gb: u64, res: &crd::PodResources) -> Vec<(crate::quota::Dim, u64)> {
    use crate::quota::{mebibytes, millicores, Dim};
    vec![
        (Dim::Workspaces, 1),
        (Dim::DiskGb, quota_gb),
        (Dim::Cpu, millicores(&res.cpu_limit).div_ceil(1000)),
        (Dim::MemoryGb, mebibytes(&res.memory_limit).div_ceil(1024)),
    ]
}

/// The same for an environment: every service gets the env unit, one definition in `k8s`.
fn environment_cost(quota_gb: u64, services: usize) -> Vec<(crate::quota::Dim, u64)> {
    use crate::quota::{mebibytes, millicores, Dim};
    let unit = crate::k8s::env_unit_resources();
    let n = services as u64;
    vec![
        (Dim::Environments, 1),
        (Dim::DiskGb, quota_gb),
        (Dim::Cpu, (n * millicores(&unit.cpu_limit)).div_ceil(1000)),
        (Dim::MemoryGb, (n * mebibytes(&unit.memory_limit)).div_ceil(1024)),
    ]
}
```

- [ ] **Step 4: Call it from all eight handlers**

Each call goes **immediately before the object is written** and after the owner, team, quota and resources have been resolved — so a refusal leaves nothing behind, and the numbers charged are the numbers stored.

In `create_ws`, after `refuse_taken_name(...)` and before `let id = rid("ws");`:

```rust
    let quota_gb = clamp_quota(body.quota_gb);
    let owner_of = if team.is_empty() { owner.clone() } else { team.clone() };
    guard_alloc(&s, &owner_of, !team.is_empty(), &workspace_cost(quota_gb, &Default::default())).await?;
```

and change the spec below to `storage: Some(crd::WorkspaceStorage { quota_gb, source })`.

In `clone_ws`, after `let quota = storage_quota(c, &src.spec.storage, &volume).await;` and before `create_workspace`:

```rust
    let owner_of = if src.spec.team.is_empty() { owner.clone() } else { src.spec.team.clone() };
    guard_alloc(&s, &owner_of, !src.spec.team.is_empty(), &workspace_cost(quota, &src.spec.resources)).await?;
```

In `restore_ws`, after `quota`, `resources` and `team` are all resolved and before `let new_id = rid("ws");`:

```rust
    // A restore is an allocation like any other: the snapshot survives the refusal untouched, so
    // the person can raise their quota and try the same id again.
    let owner_of = if team.is_empty() { owner.clone() } else { team.clone() };
    guard_alloc(&s, &owner_of, !team.is_empty(), &workspace_cost(quota, &resources)).await?;
```

In `create_env`, after `let owner = resolve_new_owner(...)` and before `let id = rid("env");`:

```rust
    let quota_gb = clamp_quota(body.quota_gb);
    guard_alloc(&s, &owner, owner != caller_id, &environment_cost(quota_gb, body.services.len())).await?;
```

and change the spec to `storage: Some(crd::WorkspaceStorage { quota_gb, source: None })`.

In `clone_env`, after the interrupted-source refusal and before `create_environment`:

```rust
    guard_alloc(&s, &src.spec.owner, src.spec.owner != caller_id, &environment_cost(quota, src.spec.services.len())).await?;
```

In `restore_env`, after the owner, services and quota are resolved and before the environment is created — same shape, using that handler's own resolved variable names (`owner`, `services`, `quota`):

```rust
    guard_alloc(&s, &owner, owner != caller_id, &environment_cost(quota, services.len())).await?;
```

In `push_ws`, after `let volume = ws_volume(&w)…` and before `create_snapshot`:

```rust
    let owner_of = if w.spec.team.is_empty() { w.spec.owner.clone() } else { w.spec.team.clone() };
    guard_alloc(&s, &owner_of, !w.spec.team.is_empty(), &[(crate::quota::Dim::Snapshots, 1)]).await?;
```

In `push_env`, likewise before `create_snapshot`:

```rust
    guard_alloc(&s, &e.spec.owner, e.spec.owner != caller_id, &[(crate::quota::Dim::Snapshots, 1)]).await?;
```

Add this comment above `guard_alloc`'s definition, because two of the spec's enforcement points have no route yet:

```rust
// The design doc also lists "changing a volume's quota" and "changing resources". Neither has a
// route today (`/v1` has no resize and no resources patch — `patch_ws_packages` is packages only),
// so there is nothing to gate. A future resize route calls THIS function with the DELTA, never a
// check of its own: the sentence and the read-then-write window are decided here.
```

- [ ] **Step 5: Delete the C2 stopgap**

```bash
rg -n "C2|creation cap|MAX_WORKSPACES|per-owner cap" crates/workspaces/src crates/workspaces/tests
```

Remove the constant, the check in `create_ws`/`create_env`, its doc comment and the tests that pin it. The `Quota` object is the cap now; two caps that can disagree is the bug. If the search finds nothing, the review plans have not landed that item — record that in the commit body and move on.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: PASS. If an existing `api_user.rs`/`api_teams.rs` create test now 404s on `{API}/quotas/...` or on a listing, add the four listing routes and the two `not_found` quota routes to that test's `routes` vec — the guard reads them on every create, and a missing mock is the mock's gap, not a behaviour change.

- [ ] **Step 7: Run the gates and commit**

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
git add crates/workspaces/src/api.rs crates/workspaces/tests
git commit -m "Refuse over-quota allocation in /v1 and drop the interim creation cap"
```

---

### Task 4: `Directory::team_role` and the quota-request routes

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `trait Directory`, the request routes and handlers
- Modify: `bins/api/src/main.rs` — `Dir::team_role`
- Modify: every `Directory` stub (`crates/workspaces/tests/api_teams.rs`, `crates/workspaces/tests/api_user.rs`, the in-module stub in `api.rs`)
- Test: `crates/workspaces/tests/api_quota.rs`

**Interfaces:**
- Consumes: `crd::{QuotaRequest, QuotaRequestSpec, RequestedQuota, RequestState, QuotaRequestStatus}` (Task 1); `api::may_act_on`, `api::teams_for`.
- Produces:
  - `api::TeamRole { Member, Admin, Owner }` — `#[derive(PartialOrd, Ord)]` in that order, so `role >= TeamRole::Admin` IS the rank rule and there is no separate `rank()` to keep in step.
  - `Directory::team_role(&self, user: &str, team: &str) -> Option<TeamRole>` — a required trait method.
  - `POST /v1/quota-requests`, `GET /v1/quota-requests`.

- [ ] **Step 1: Write the failing tests**

Add to the stub in `crates/workspaces/tests/api_quota.rs` (extend the `StubMembership` copied from `api_teams.rs`):

```rust
    async fn team_role(&self, user: &str, team: &str) -> Option<TeamRole> {
        match (user, team) {
            ("karthik", "acme") => Some(TeamRole::Admin),
            ("bob", "acme") => Some(TeamRole::Member),
            _ => None,
        }
    }
```

and make `teams_for` answer `vec!["acme".into()]` for both `karthik` and `bob`. Then:

```rust
fn req_obj(name: &str, owner: &str, state: Option<&str>) -> Value {
    let mut o = json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "QuotaRequest",
        "metadata": {"name": name, "labels": {"rustic-git.io/owner": owner}},
        "spec": {"owner": owner, "requested": {"workspaces": 10}, "reason": "more room"}
    });
    if let Some(st) = state {
        o["status"] = json!({"state": st});
    }
    o
}

/// A team admin may ask on the team's behalf; the object's owner is the TEAM, and the label is a
/// view of it.
#[tokio::test]
async fn a_team_admin_may_open_a_request_for_the_team() {
    let routes = vec![
        get(format!("{API}/quotarequests"), list_of("QuotaRequest", vec![])),
        post(format!("{API}/quotarequests"), req_obj("qr-1", "acme", None)),
    ];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/quota-requests", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"owner": "acme", "requested": {"workspaces": 40}, "reason": "onboarding"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
    let sent = s.rec.sent("POST", &format!("{API}/quotarequests")).remove(0);
    assert_eq!(sent["spec"]["owner"], "acme");
    assert_eq!(sent["spec"]["requested"]["workspaces"], 40);
    assert_eq!(sent["metadata"]["labels"]["rustic-git.io/owner"], "acme");
}

/// A plain member may not: raising a team's ceiling is a team decision, and the message says so.
#[tokio::test]
async fn a_plain_member_may_not_open_a_team_request() {
    let s = server(true, vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/quota-requests", s.base))
        .bearer_auth(token(&s.jwt, "bob"))
        .json(&json!({"owner": "acme", "requested": {"workspaces": 40}, "reason": "please"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(resp.text().await.unwrap(), "only a team admin can request a team quota");
}

/// One pending request per owner. A request with no status yet counts as pending — /v1 creates the
/// object and stamps status separately, and that window must not read as "decided".
#[tokio::test]
async fn a_second_pending_request_is_refused() {
    let routes = vec![get(
        format!("{API}/quotarequests"),
        list_of("QuotaRequest", vec![req_obj("qr-1", "karthik", Some("pending"))]),
    )];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/quota-requests", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"requested": {"workspaces": 10}, "reason": "again"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text().await.unwrap(), "a request is already pending");
}

/// A decided one is not in the way: the same owner may ask again after a denial.
#[tokio::test]
async fn a_denied_request_does_not_block_the_next_one() {
    let routes = vec![
        get(format!("{API}/quotarequests"), list_of("QuotaRequest", vec![req_obj("qr-1", "karthik", Some("denied"))])),
        post(format!("{API}/quotarequests"), req_obj("qr-2", "karthik", None)),
    ];
    let s = server(true, routes).await;
    let code = reqwest::Client::new()
        .post(format!("{}/v1/quota-requests", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"requested": {"workspaces": 10}, "reason": "again"}))
        .send().await.unwrap()
        .status();
    assert_eq!(code, 201);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p rustic-git-workspaces --test api_quota -- --test-threads=1; echo exit=$?`
Expected: FAIL — `TeamRole` and `team_role` do not exist (compile error), then 404 on the routes.

- [ ] **Step 3: Implement the trait method**

In `crates/workspaces/src/api.rs`, beside `OwnerMaterial`:

```rust
/// A person's standing in a team, as the platform directory records it.
///
/// A local enum rather than `rustic_git_pulls::directory::Role` for the same reason the whole
/// `Directory` trait is local: this crate must not depend on the mongo-backed one just for a
/// lookup. `Ord` is declared by the variant ORDER — `Member < Admin < Owner` — so `>= Admin` is
/// the rank rule, and there is no second rank table to fall out of step with the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TeamRole {
    Member,
    Admin,
    Owner,
}
```

and on the trait:

```rust
    /// The caller's role in `team`, or `None` when they are not a member — or when the lookup
    /// could not be made. Both answer "no" here, which is the safe direction for the one decision
    /// it feeds: who may raise a team's ceiling.
    ///
    /// `user` is whatever identity `teams_for` matches on, so the two can never disagree about who
    /// is in the team.
    async fn team_role(&self, user: &str, team: &str) -> Option<TeamRole>;
```

In `bins/api/src/main.rs`, on `impl Directory for Dir`:

```rust
    async fn team_role(&self, user: &str, team: &str) -> Option<rustic_git_workspaces::api::TeamRole> {
        use rustic_git_pulls::directory::Role;
        use rustic_git_workspaces::api::TeamRole;
        let t = self.0.get(team).await.ok().flatten()?;
        // The same `user` value `slugs_for` matches on, through the same members array — one
        // identity, so membership and role can never disagree.
        match rustic_git_pulls::directory::Directory::role_of(&t, user)? {
            Role::Owner => Some(TeamRole::Owner),
            Role::Admin => Some(TeamRole::Admin),
            Role::Member => Some(TeamRole::Member),
        }
    }
```

(`Directory::get` and `Directory::role_of` are `crates/pulls/src/directory/teams.rs`; `role_of` is an associated function taking `&Team`.)

Then `cargo check -p rustic-git-workspaces --all-targets` and add `team_role` to every stub the compiler names — a stub whose case does not exercise roles returns `None` with a one-line comment saying so.

- [ ] **Step 4: Implement the routes**

In `router()`:

```rust
        .route("/v1/quota-requests", post(create_quota_request).get(list_quota_requests))
```

and the handlers:

```rust
#[derive(serde::Deserialize)]
struct NewQuotaRequest {
    /// Absent means the caller's own quota.
    #[serde(default)]
    owner: Option<String>,
    requested: crd::RequestedQuota,
    #[serde(default)]
    reason: String,
}

/// Who may ask, and for whom.
///
/// A person may always ask for their own. A team's ceiling is a team decision, so only a member
/// whose directory role is at least admin may ask on its behalf — checked against the DIRECTORY,
/// never against a label and never against who happens to have created something.
async fn may_request_for(s: &ApiState, c: &Caller, owner: &str) -> Result<(), Response> {
    if owner == c.name {
        return Ok(());
    }
    let Some(dir) = &s.directory else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "team lookup not configured on this node").into_response());
    };
    match dir.team_role(&c.name, owner).await {
        Some(r) if r >= TeamRole::Admin => Ok(()),
        // A member gets the reason; a non-member learns nothing about the team at all.
        Some(_) => Err((StatusCode::FORBIDDEN, "only a team admin can request a team quota").into_response()),
        None => Err(not_found()),
    }
}

/// Every request of `owner`, newest first, label-selected — and re-checked against `spec.owner`,
/// because the label is a view.
async fn requests_of(c: &kube::Client, owner: &str) -> Result<Vec<crd::QuotaRequest>, Response> {
    let api: Api<crd::QuotaRequest> = Api::all(c.clone());
    Ok(api
        .list(&owned_by(owner))
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|r| r.spec.owner == owner)
        .collect())
}

/// A request with no status yet is PENDING: `/v1` writes the object and stamps status in a second
/// call, and reading that window as "decided" would let two requests stand at once.
fn is_pending(r: &crd::QuotaRequest) -> bool {
    r.status.as_ref().map(|s| s.state).unwrap_or(crd::RequestState::Pending) == crd::RequestState::Pending
}

async fn create_quota_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewQuotaRequest>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let owner = body.owner.unwrap_or_else(|| c.name.clone());
    may_request_for(&s, &c, &owner).await?;
    let client = kube(&s)?;
    // One at a time, so the queue is a list of decisions rather than a list of the same ask.
    if requests_of(client, &owner).await?.iter().any(is_pending) {
        return Err((StatusCode::CONFLICT, "a request is already pending").into_response());
    }
    let id = rid("qr");
    let mut r = crd::QuotaRequest::new(&id, crd::QuotaRequestSpec {
        owner: owner.clone(),
        requested: body.requested,
        reason: body.reason,
    });
    // A view of `spec.owner`, so the queue and the owner's own list are indexed selectors — same
    // rule as every other label in this codebase.
    r.metadata.labels = Some(BTreeMap::from([(OWNER_LABEL.to_string(), owner)]));
    let api: Api<crd::QuotaRequest> = Api::all(client.clone());
    let made = api.create(&PostParams::default(), &r).await.map_err(kube_err)?;
    Ok((StatusCode::CREATED, Json(request_doc(&made))).into_response())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaRequestDoc {
    id: String,
    owner: String,
    requested: crd::RequestedQuota,
    reason: String,
    state: crd::RequestState,
    decided_by: Option<String>,
    decided_at: Option<String>,
    note: Option<String>,
    created_at: Option<String>,
}

fn request_doc(r: &crd::QuotaRequest) -> QuotaRequestDoc {
    let st = r.status.clone().unwrap_or_default();
    QuotaRequestDoc {
        id: r.name_any(),
        owner: r.spec.owner.clone(),
        requested: r.spec.requested.clone(),
        reason: r.spec.reason.clone(),
        state: st.state,
        decided_by: st.decided_by,
        decided_at: st.decided_at,
        note: st.note,
        created_at: r.metadata.creation_timestamp.as_ref().map(|t| t.0.to_rfc3339()),
    }
}

#[derive(serde::Deserialize)]
struct RequestQuery {
    #[serde(default)]
    owner: Option<String>,
}

/// The caller's own requests and their teams'. A superadmin with no `owner` given gets the whole
/// queue — that is the admin page's list, and it is the claim that grants it, never an ownership.
async fn list_quota_requests(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<RequestQuery>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    let client = kube(&s)?;
    let mut rows = Vec::new();
    match q.owner {
        Some(owner) => {
            if !may_act_on(&s, &c, &owner).await {
                return Err(not_found());
            }
            rows.extend(requests_of(client, &owner).await?);
        }
        None if c.superadmin => {
            let api: Api<crd::QuotaRequest> = Api::all(client.clone());
            rows.extend(api.list(&ListParams::default()).await.map_err(kube_err)?.items);
        }
        None => {
            for owner in caller_owners(&s, &c).await {
                rows.extend(requests_of(client, &owner).await?);
            }
        }
    }
    rows.sort_by(|a, b| b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp));
    Ok(Json(rows.iter().map(request_doc).collect::<Vec<_>>()).into_response())
}
```

`Caller` and `c.superadmin` come from Task 5b. If 5b has not landed, write `let c = caller(...)`, use `c` where `c.name` appears, and make the `None` arm the `caller_owners` branch only — 5b adds the superadmin arm in its own commit.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: PASS.

- [ ] **Step 6: Run the gates and commit**

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
git add crates/workspaces/src/api.rs crates/workspaces/tests bins/api/src/main.rs
git commit -m "Let an owner or a team admin open one pending quota request"
```

---

### Task 5a: The superadmin list, the bootstrap, and the JWT claim

**Files:**
- Modify: `crates/core/src/jwt.rs` — `Claims.superadmin`, `mint_admin`
- Modify: `crates/pulls/src/directory/mod.rs` — the `superadmins` collection and its four methods
- Modify: `crates/api/src/teams.rs` — mint the claim at both mint sites; `POST`/`DELETE /api/admin/superadmins/{user}`
- Modify: `crates/api/src/lib.rs` — mount those two routes
- Modify: `bins/api/src/main.rs` — bootstrap from `RUSTIC_GIT_WORKSPACES_ADMINS` at boot
- Test: `crates/core/src/jwt.rs`'s existing test module

**Interfaces:**
- Produces:
  - `jwt::Claims.superadmin: bool` (defaults false, omitted from the token when false)
  - `Jwt::mint_admin(&self, email, name, username: Option<&str>, superadmin: bool) -> Result<String>`; `Jwt::mint` keeps its exact current signature and delegates with `false`
  - `directory::Directory::{is_superadmin, superadmins, add_superadmin, remove_superadmin, ensure_superadmins}`
- Consumes: nothing from earlier tasks.

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/jwt.rs`'s `mod tests`:

```rust
    /// The claim rides in the session token, so `/v1` and the web both read one fact minted in one
    /// place. It is OMITTED when false: an ordinary token must not carry a field that only ever
    /// says no, and an old token without it must verify as an ordinary user rather than fail.
    #[test]
    fn the_superadmin_claim_round_trips_and_defaults_off() {
        let j = jwt();
        let plain = j.mint("a@b.c", "A", Some("a")).unwrap();
        assert!(!j.verify(&plain).unwrap().superadmin);
        let payload = plain.split('.').nth(1).unwrap();
        let raw = String::from_utf8(
            base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload).unwrap(),
        )
        .unwrap();
        assert!(!raw.contains("superadmin"), "{raw}");

        let admin = j.mint_admin("a@b.c", "A", Some("a"), true).unwrap();
        assert!(j.verify(&admin).unwrap().superadmin);
        // A CLI token never carries it: `kl` is not an admin console.
        let (c, _) = j.verify_any_user(&j.mint_cli("a@b.c", "A", Some("a")).unwrap().0).unwrap();
        assert!(!c.superadmin);
    }
```

Adjust the `mint_cli` call to whatever that function is actually named and returns in `jwt.rs` (read it — the CLI mint is right above `verify_any_user`); the assertion that matters is `!c.superadmin`. Use whichever base64 crate `crates/core` already depends on; if none is handy, drop the raw-payload assertion and keep the two `verify` assertions plus `serde_json::to_value(&claims)` showing no `superadmin` key.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p rustic-git-core jwt; echo exit=$?`
Expected: FAIL — no `superadmin` field, no `mint_admin`.

- [ ] **Step 3: Implement the claim**

In `crates/core/src/jwt.rs`, on `Claims`:

```rust
    /// Platform-wide administrator. A CLAIM, never an ownership: it says who may act, never who
    /// owns anything, and it is minted only at sign-in from the directory's own list. Omitted when
    /// false so an ordinary token carries no field that only ever says no, and so every token
    /// minted before this existed keeps verifying.
    #[serde(default, skip_serializing_if = "is_false")]
    pub superadmin: bool,
```

and beside it:

```rust
fn is_false(b: &bool) -> bool {
    !*b
}
```

Replace `mint`'s body and add the new entry point:

```rust
    /// An ordinary session. Kept at this exact signature because every existing caller uses it and
    /// an admin flag is not something a caller should be able to pass by accident.
    pub fn mint(&self, email: &str, name: &str, username: Option<&str>) -> Result<String> {
        self.mint_admin(email, name, username, false)
    }

    /// The same, carrying the platform-administrator claim. One caller: the sign-in path, which is
    /// the only place that has asked the directory.
    pub fn mint_admin(&self, email: &str, name: &str, username: Option<&str>, superadmin: bool) -> Result<String> {
        let now = now()?;
        let claims = Claims {
            sub: email.trim().to_lowercase(),
            name: name.to_string(),
            username: username.map(str::to_string),
            typ: "session".into(),
            superadmin,
            iat: now,
            exp: now + TTL_SECS,
        };
        encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|e| err(format!("minting token: {e}")))
    }
```

In `verify_any_user`, the `Claims` it builds from a `CliClaims` gains `superadmin: false`, with:

```rust
                // A CLI token never carries the claim: `kl` is a workspace tool, not an admin
                // console, and a 30-day credential is the wrong life for one.
                superadmin: false,
```

- [ ] **Step 4: The directory collection**

In `crates/pulls/src/directory/mod.rs`, beside `User`:

```rust
/// A platform administrator. One row per person, keyed by email — the same identity the session
/// token's `sub` carries, so the mint is a single lookup.
///
/// A collection rather than an env var because the env var could only ever be a bootstrap: it is
/// read by one process at boot, cannot be changed without a roll, and says nothing about who
/// granted it. `addedBy` is the audit trail the env var never had.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SuperAdmin {
    #[serde(rename = "_id")]
    pub user: String,
    pub added_at: DateTime,
    pub added_by: String,
}
```

Add `superadmins: Collection<SuperAdmin>` to `struct Directory` and `superadmins: db.collection("superadmins"),` in `connect`. Then, in the `impl Directory` block:

```rust
    pub async fn is_superadmin(&self, user: &str) -> Result<bool> {
        Ok(self
            .superadmins
            .find_one(doc! { "_id": user.trim().to_lowercase() })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?
            .is_some())
    }

    pub async fn superadmins(&self) -> Result<Vec<SuperAdmin>> {
        use futures::TryStreamExt;
        self.superadmins
            .find(doc! {})
            .await
            .map_err(|e| err(format!("mongo: {e}")))?
            .try_collect()
            .await
            .map_err(|e| err(format!("mongo: {e}")))
    }

    /// Idempotent: granting twice is not an error, and it must not rewrite who granted it first.
    pub async fn add_superadmin(&self, user: &str, by: &str) -> Result<()> {
        let user = user.trim().to_lowercase();
        let row = SuperAdmin { user: user.clone(), added_at: DateTime::now(), added_by: by.to_string() };
        self.superadmins
            .update_one(doc! { "_id": &user }, doc! { "$setOnInsert": mongodb::bson::to_document(&row).map_err(|e| err(format!("bson: {e}")))? })
            .upsert(true)
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        Ok(())
    }

    pub async fn remove_superadmin(&self, user: &str) -> Result<()> {
        self.superadmins
            .delete_one(doc! { "_id": user.trim().to_lowercase() })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        Ok(())
    }

    /// The `RUSTIC_GIT_WORKSPACES_ADMINS` bootstrap, run once at boot. It only ever ADDS: the env
    /// is a way to get the first administrator into an empty cluster, not the list itself, so
    /// removing an email from it must not silently revoke someone the list has since granted.
    pub async fn ensure_superadmins(&self, emails: &[String]) -> Result<usize> {
        let mut n = 0;
        for e in emails {
            if !self.is_superadmin(e).await? {
                self.add_superadmin(e, "bootstrap").await?;
                n += 1;
            }
        }
        Ok(n)
    }
```

- [ ] **Step 5: Mint the claim at sign-in**

In `crates/api/src/teams.rs`, at **both** mint sites (`upsert_user` around :110 and `claim_username` around :155), replace `j.mint(&u.email, &u.name, u.username.as_deref())` with a lookup first:

```rust
            // Asked once, at the mint. The token is the only thing every later caller reads, so a
            // grant or a revocation takes effect on the next sign-in and nowhere else — which is
            // also its whole revocation story: the session's 12 h life is the window.
            let admin = db.is_superadmin(&u.email).await.unwrap_or(false);
            let token = match api.jwt.as_deref() {
                Some(j) => match j.mint_admin(&u.email, &u.name, u.username.as_deref(), admin) {
```

(`db` is already in scope at both sites; keep the rest of each `match` unchanged.)

- [ ] **Step 6: The two management routes**

In `crates/api/src/teams.rs`:

```rust
/// `POST`/`DELETE /api/admin/superadmins/{user}` — the list manages itself.
///
/// Superadmin-only, and it reads the CALLER's row rather than their token: this is the one surface
/// where a 12-hour-old claim is not good enough, because a revoked administrator holding a valid
/// token must not be able to grant themselves back.
async fn require_superadmin(api: &Arc<Api>, headers: &axum::http::HeaderMap) -> Result<String, Response> {
    let caller = caller(api, headers)?;
    let db = directory(api)?;
    match db.is_superadmin(&caller).await {
        Ok(true) => Ok(caller),
        Ok(false) => Err((StatusCode::FORBIDDEN, "admin only").into_response()),
        Err(e) => Err(db_err("check admin", &caller, e)),
    }
}

pub(crate) async fn add_superadmin(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(user): axum::extract::Path<String>,
) -> Response {
    let by = match require_superadmin(&api, &headers).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.add_superadmin(&user, &by).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_err("grant admin", &user, e),
    }
}

pub(crate) async fn remove_superadmin(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(user): axum::extract::Path<String>,
) -> Response {
    if let Err(r) = require_superadmin(&api, &headers).await {
        return r;
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.remove_superadmin(&user).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_err("revoke admin", &user, e),
    }
}

pub(crate) async fn list_superadmins(State(api): State<Arc<Api>>, headers: axum::http::HeaderMap) -> Response {
    if let Err(r) = require_superadmin(&api, &headers).await {
        return r;
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.superadmins().await {
        Ok(rows) => axum::Json(rows).into_response(),
        Err(e) => db_err("list admins", "", e),
    }
}
```

In `crates/api/src/lib.rs`, beside the other `/api/...` routes:

```rust
        .route("/api/admin/superadmins", get(crate::teams::list_superadmins))
        .route(
            "/api/admin/superadmins/{user}",
            post(crate::teams::add_superadmin).delete(crate::teams::remove_superadmin),
        )
```

Match the surrounding router's exact builder style and imports; if the browse routes are mounted on the peer listener only, mount these on the **same listener the other `/api/` team routes use** — they are a signed-in-user surface, not a peer one.

- [ ] **Step 7: The boot bootstrap**

In `bins/api/src/main.rs`, right after the `directory` is constructed (inside the `Ok(uri)` arm, after `tracing::info!(db = %db, …)`):

```rust
            // `RUSTIC_GIT_WORKSPACES_ADMINS` is a BOOTSTRAP now, not the list: it seeds the
            // directory once so an empty cluster has a first administrator, and after that the
            // list is managed through /api/admin/superadmins. Additive only — dropping an address
            // from the env must not silently revoke someone.
            let seed: Vec<String> = std::env::var("RUSTIC_GIT_WORKSPACES_ADMINS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            match d.ensure_superadmins(&seed).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(added = n, "superadmins seeded from RUSTIC_GIT_WORKSPACES_ADMINS"),
                Err(e) => tracing::warn!(error = %e, "superadmin bootstrap skipped"),
            }
```

- [ ] **Step 8: Run the tests and gates**

```bash
cargo test -p rustic-git-core -p rustic-git-api -p rustic-git-pulls -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
```
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/jwt.rs crates/pulls/src/directory/mod.rs crates/api/src/teams.rs crates/api/src/lib.rs bins/api/src/main.rs
git commit -m "Mint a superadmin claim from a directory list bootstrapped by the admins env"
```

---

### Task 5b: `/v1` reads the claim — `Caller`, `may_act_on`, approve and deny

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `Caller`, `caller`, `may_act_on`, `require_admin`, `ApiState`, `create_region`, the approve/deny routes, and every call site the compiler names
- Modify: `bins/api/src/main.rs` — `ApiState::new` loses its `admins` argument
- Modify: the six test harnesses that call `ApiState::new`
- Test: `crates/workspaces/tests/api_quota.rs`

**Interfaces:**
- Consumes: `jwt::Claims.superadmin` (Task 5a); `crd::{QuotaRequest, RequestState, QuotaSpec}` (Task 1); `quota::effective` (Task 2); `TeamRole` (Task 4).
- Produces:
  - `api::Caller { pub name: String, pub superadmin: bool }` with `Deref<Target = str>` and `Display` (so `&caller`, `format!("{caller}")` and `caller.as_str()` all work at existing sites)
  - `api::caller(...) -> Result<Caller, Response>` (same name, new return type)
  - `api::require_admin(c: &Caller) -> Result<(), Response>`
  - `api::may_act_on(s, c: &Caller, owner: &str) -> bool` — third arm is the claim
  - `POST /v1/quota-requests/{id}/approve`, `POST /v1/quota-requests/{id}/deny`
  - `ApiState::new(store, jwt)` — two arguments; the `admins` field is gone

- [ ] **Step 1: Write the failing tests**

Add to `crates/workspaces/tests/api_quota.rs` (and add an `admin_token` helper beside `token`):

```rust
/// A superadmin token, minted the way the api tier mints one at sign-in.
fn admin_token(jwt: &Jwt) -> String {
    jwt.mint_admin("root@example.com", "Root", Some("root"), true).unwrap()
}

/// The claim's third arm on `may_act_on`: support can read another owner's objects without
/// impersonating them, and the access is logged with the caller.
#[tokio::test]
async fn a_superadmin_may_list_another_owners_workspaces() {
    let routes = vec![
        get(format!("{API}/snapshots"), list_of("Snapshot", vec![])),
        get(format!("{API}/workspaces"), list_of("Workspace", vec![ws_obj("ws-1", "karthik", "running")])),
    ];
    let s = server(true, routes).await;
    let code = reqwest::Client::new()
        .get(format!("{}/v1/workspaces?owner=karthik", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .send().await.unwrap()
        .status();
    assert_eq!(code, 200);
}

/// Approving writes the Quota FIRST and only then marks the request: a request marked approved
/// whose quota never landed is the one ordering that leaves a person told yes and still refused.
#[tokio::test]
async fn approving_writes_the_quota_then_marks_the_request() {
    let routes = vec![
        get(format!("{API}/quotarequests/qr-1"), req_obj("qr-1", "karthik", Some("pending"))),
        not_found(format!("{API}/quotas/karthik")),
        not_found(format!("{API}/quotas/default-user")),
        post(format!("{API}/quotas"), json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "karthik"},
            "spec": {"workspaces": 10, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 8, "memoryGb": 32}
        })),
        Route { method: "PATCH", path: format!("{API}/quotarequests/qr-1/status"), status: 200, body: req_obj("qr-1", "karthik", Some("approved")) },
    ];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/quota-requests/qr-1/approve", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "ok"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());

    let written = s.rec.sent("POST", &format!("{API}/quotas")).remove(0);
    // Only the dimension the request named moved; the other five stay at the default they had.
    assert_eq!(written["spec"]["workspaces"], 10);
    assert_eq!(written["spec"]["environments"], 2);
    let calls = s.rec.calls();
    let quota_at = calls.iter().position(|c| c == &format!("POST {API}/quotas")).expect("quota written");
    let mark_at = calls.iter().position(|c| c.contains("quotarequests/qr-1/status")).expect("request marked");
    assert!(quota_at < mark_at, "the quota must land before the request is marked: {calls:?}");
}

/// Deciding is the claim's, not the owner's: the person who asked cannot approve their own.
#[tokio::test]
async fn an_owner_may_not_approve_their_own_request() {
    let s = server(true, vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/quota-requests/qr-1/approve", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(resp.text().await.unwrap(), "admin only");
}

/// A request that is already decided is not re-decidable: the record of who said what stands.
#[tokio::test]
async fn a_decided_request_cannot_be_decided_again() {
    let routes = vec![get(format!("{API}/quotarequests/qr-1"), req_obj("qr-1", "karthik", Some("denied")))];
    let s = server(true, routes).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/quota-requests/qr-1/deny", s.base))
        .bearer_auth(admin_token(&s.jwt))
        .json(&json!({"note": "no"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.text().await.unwrap(), "that request has already been decided");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p rustic-git-workspaces --test api_quota -- --test-threads=1; echo exit=$?`
Expected: FAIL — `mint_admin` is in scope (Task 5a) but the routes and the claim arm are not.

- [ ] **Step 3: Introduce `Caller`**

In `crates/workspaces/src/api.rs`:

```rust
/// Who is calling, and whether they hold the platform-administrator claim.
///
/// A struct rather than the bare handle because two facts travel together everywhere: the owner
/// name every path is scoped by, and the claim `may_act_on` reads as its third arm. `Deref` and
/// `Display` are so the ~30 sites that only want the handle read unchanged.
#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    /// A CLAIM from the session token, minted at sign-in from the directory's list. Never an
    /// ownership: it decides who may act, never who owns anything, and it never widens a quota.
    pub superadmin: bool,
}

impl std::ops::Deref for Caller {
    type Target = str;
    fn deref(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}
```

Change `caller()`'s tail to build one:

```rust
    let superadmin = c.superadmin;
    let name = c.username.filter(|u| !u.is_empty()).ok_or_else(|| {
        (StatusCode::FORBIDDEN, "pick a username before using workspaces").into_response()
    })?;
    Ok(Caller { name, superadmin })
```

and its return type to `Result<Caller, Response>`.

`may_act_on` and `require_admin`:

```rust
/// `owner` is the object's actual owner field (a username or a team slug). Their own always
/// passes; a team's passes for a member; and a platform administrator passes for anyone —
/// so support can clean up without impersonating the person.
async fn may_act_on(s: &ApiState, c: &Caller, owner: &str) -> bool {
    if c.name == owner {
        return true;
    }
    if teams_for(s, &c.name).await.iter().any(|t| t == owner) {
        return true;
    }
    if c.superadmin {
        // Every cross-owner access a claim allows is recorded with the caller: the point of the
        // claim is that support never has to impersonate, and an un-logged one would be worse than
        // impersonation, not better.
        tracing::info!(caller = %c.name, %owner, "superadmin acting on another owner");
        return true;
    }
    false
}

/// The admin gate, now the CLAIM rather than an email allowlist. `/v1/regions` moved under it too.
fn require_admin(c: &Caller) -> Result<(), Response> {
    if c.superadmin {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "admin only").into_response())
    }
}
```

Delete `ApiState.admins`, the `admins` parameter of `ApiState::new`, the `use std::collections::HashSet` if it becomes unused, and rewrite the module doc's paragraph about the static allowlist:

```rust
//! Admin-gated routes (`/v1/regions` and the quota decisions) read the `superadmin` claim on the
//! session token, minted at sign-in from the directory's own list. The static email allowlist this
//! used to carry is gone; `RUSTIC_GIT_WORKSPACES_ADMINS` is a bootstrap for that list and nothing
//! reads it here.
```

Rewrite `create_region`'s gate to use it:

```rust
    // The claim, not an email: a region write is a platform decision, so it goes through the same
    // gate the quota decisions do. An administrator now needs a handle like everyone else, which
    // is what `caller` enforces — every admin surface is reached from a signed-in session.
    let c = caller(&s, &headers).await?;
    require_admin(&c)?;
```

- [ ] **Step 4: Sweep the call sites**

```bash
cargo check -p rustic-git-workspaces --all-targets 2>&1 | head -80
```

Fix each error by class, and nothing else:
- `expected &str, found Caller` on a call taking `&str` → pass `&c` (deref coercion) or `&c.name`.
- `expected String, found Caller` where a value is stored into a spec → `c.name.clone()`.
- `binary operation == cannot be applied` → compare `c.name == owner`.
- `may_act_on(&s, &caller_id, …)` where `caller_id` is a `&str` from somewhere other than `caller()` → thread the `Caller` down instead; `find_env`, `my_ws`, `resolve_new_owner`, `caller_owners` and `find_commit_model_snapshot*` all take the caller and become `&Caller`.
- Test harnesses calling `ApiState::new(store, jwt, HashSet::new())` → drop the third argument.

- [ ] **Step 5: Approve and deny**

In `router()`:

```rust
        .route("/v1/quota-requests/{id}/approve", post(approve_quota_request))
        .route("/v1/quota-requests/{id}/deny", post(deny_quota_request))
```

Handlers:

```rust
#[derive(serde::Deserialize, Default)]
struct Decision {
    #[serde(default)]
    note: Option<String>,
}

/// Overlay a request onto a limit. Only the dimensions the request NAMED move; approving must not
/// silently reset a limit somebody has already granted on another axis.
fn overlay(base: crd::QuotaSpec, want: &crd::RequestedQuota) -> crd::QuotaSpec {
    crd::QuotaSpec {
        workspaces: want.workspaces.unwrap_or(base.workspaces),
        environments: want.environments.unwrap_or(base.environments),
        snapshots: want.snapshots.unwrap_or(base.snapshots),
        disk_gb: want.disk_gb.unwrap_or(base.disk_gb),
        cpu: want.cpu.unwrap_or(base.cpu),
        memory_gb: want.memory_gb.unwrap_or(base.memory_gb),
    }
}

async fn pending_request(s: &ApiState, id: &str) -> Result<crd::QuotaRequest, Response> {
    let api: Api<crd::QuotaRequest> = Api::all(kube(s)?.clone());
    let r = api.get_opt(id).await.map_err(kube_err)?.ok_or_else(not_found)?;
    if !is_pending(&r) {
        return Err((StatusCode::CONFLICT, "that request has already been decided").into_response());
    }
    Ok(r)
}

/// Stamp the outcome. `status`, not spec: the request is what was asked, the decision is what
/// happened to it, and only this tier ever writes it (no controller reconciles a request).
async fn decide(s: &ApiState, id: &str, state: crd::RequestState, by: &str, note: Option<String>) -> Result<Response, Response> {
    let api: Api<crd::QuotaRequest> = Api::all(kube(s)?.clone());
    let patch = serde_json::json!({"status": {
        "state": state,
        "decidedBy": by,
        "decidedAt": chrono::Utc::now().to_rfc3339(),
        "note": note,
    }});
    let out = api
        .patch_status(id, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(kube_err)?;
    Ok(Json(request_doc(&out)).into_response())
}

/// Approve: write the `Quota` FIRST, then mark the request.
///
/// That order, always. A request marked approved whose quota never landed leaves a person told yes
/// and still refused, with nothing left pending to retry; a quota that landed under a request
/// still marked pending is merely a second approval that changes nothing.
async fn approve_quota_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    require_admin(&c)?;
    let note: Decision = if body.is_empty() { Default::default() } else {
        serde_json::from_slice(&body).map_err(|_| (StatusCode::BAD_REQUEST, "invalid body").into_response())?
    };
    let r = pending_request(&s, &id).await?;
    let owner = r.spec.owner.clone();
    let client = kube(&s)?;
    let api: Api<crd::Quota> = Api::all(client.clone());
    // The team/person split only decides WHICH default is the starting point; an owner with an
    // object of their own starts from that object either way.
    let team = teams_for(&s, &c.name).await.iter().any(|t| *t == owner) || api.get_opt(&owner).await.map_err(kube_err)?.is_none() && owner != c.name;
    let existing = api.get_opt(&owner).await.map_err(kube_err)?;
    let base = match &existing {
        Some(q) => q.spec.clone(),
        None => crate::quota::effective(client, &owner, team).await.map_err(kube_err)?,
    };
    let spec = overlay(base, &r.spec.requested);
    match existing {
        Some(_) => {
            api.patch(&owner, &PatchParams::default(), &Patch::Merge(&serde_json::json!({"spec": spec})))
                .await
                .map_err(kube_err)?;
        }
        None => {
            api.create(&PostParams::default(), &crd::Quota::new(&owner, spec)).await.map_err(kube_err)?;
        }
    }
    decide(&s, &id, crd::RequestState::Approved, &c.name, note.note).await
}

async fn deny_quota_request(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    require_admin(&c)?;
    let note: Decision = if body.is_empty() { Default::default() } else {
        serde_json::from_slice(&body).map_err(|_| (StatusCode::BAD_REQUEST, "invalid body").into_response())?
    };
    pending_request(&s, &id).await?;
    decide(&s, &id, crd::RequestState::Denied, &c.name, note.note).await
}
```

Simplify the `team` line if the compiler or a reviewer finds it convoluted: the honest rule is *"a team slug is one the directory answers for"*, so `let team = s.directory.is_some() && owner != c.name && teams_for(&s, &owner).await.is_empty();` is wrong — use instead:

```rust
    // A slug does not say which it is, so ask the directory: a team is an owner that has members.
    // Only used to pick which `default-*` object is the starting point, so a wrong guess costs a
    // fallback number, never an authorization.
    let team = s.directory.as_ref().is_some_and(|_| owner != c.name);
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p rustic-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: PASS.

- [ ] **Step 7: Run the gates and commit**

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
git add crates/workspaces bins/api/src/main.rs
git commit -m "Gate the admin surfaces on the superadmin claim and decide quota requests"
```

---

### Task 6: The agent writes a `ResourceQuota` per owner namespace

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` — `resource_quota()` builder + its unit test
- Modify: `bins/agent/src/binding.rs` — `apply_binding`
- Modify: `bins/agent/src/controller/environment.rs` — after the namespace `ensure` (around `:352`)
- Test: `crates/workspaces/src/k8s.rs`'s test module; `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `crd::QuotaSpec`, `crd::default_quota` (Task 1); `quota::effective` (Task 2).
- Produces: `k8s::resource_quota(ns: &str, owner: &str, kind: &str, q: &crd::QuotaSpec) -> ResourceQuota`.

- [ ] **Step 1: Write the failing builder test**

Append to `crates/workspaces/src/k8s.rs`'s test module:

```rust
    #[test]
    fn a_resource_quota_caps_the_namespaces_limits() {
        let rq = resource_quota("ws-alice", "alice", "workspace", &crate::crd::default_quota(false));
        let hard = rq.spec.unwrap().hard.unwrap();
        assert_eq!(hard["limits.cpu"].0, "8");
        assert_eq!(hard["limits.memory"].0, "32Gi");
        assert_eq!(rq.metadata.labels.unwrap()["rustic-git.io/owner"], "alice");
        // No ownerReference, the same reason the namespace and the LimitRange have none: the cap
        // is shared by every workspace in here and must not vanish with any one of them.
        assert!(rq.metadata.owner_references.is_none());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p rustic-git-workspaces k8s::tests::a_resource_quota; echo exit=$?`
Expected: FAIL — `resource_quota` not found.

- [ ] **Step 3: Implement the builder**

Add `ResourceQuota, ResourceQuotaSpec` to the `k8s_openapi::api::core::v1` import list in `crates/workspaces/src/k8s.rs`, then, right after `limit_range`:

```rust
/// The namespace's TOTAL ceiling, as against `limit_range`'s per-container one.
///
/// Enforced by the API server at admission, so it holds for a pod created by any path — a future
/// code path that forgets, a debug pod, an operator with kubectl. That is what makes it the hard
/// stop behind `/v1`'s read-then-write check, which can overshoot by one under concurrency.
///
/// Only cpu and memory: disk is bounded per volume by its own btrfs qgroup, and counts have no
/// Kubernetes expression at all.
///
/// ponytail: the cap is PER NAMESPACE, and a person in several teams has one namespace per team,
/// so the platform-side ceiling repeats per team while `/v1`'s count is the exact per-owner
/// number. Collapse to one namespace per team, or sum across namespaces here, if the platform side
/// ever has to be exact too.
pub fn resource_quota(ns: &str, owner: &str, kind: &str, q: &crate::crd::QuotaSpec) -> ResourceQuota {
    ResourceQuota {
        metadata: ObjectMeta {
            name: Some("owner-quota".to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels(owner, kind)),
            // None, for the same reason the namespace and the LimitRange carry none: this is the
            // shared ceiling of everything in here, not a possession of any one object.
            ..Default::default()
        },
        spec: Some(ResourceQuotaSpec {
            hard: Some(BTreeMap::from([
                ("limits.cpu".to_string(), Quantity(q.cpu.to_string())),
                ("limits.memory".to_string(), Quantity(format!("{}Gi", q.memory_gb))),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    }
}
```

- [ ] **Step 4: Write the failing agent test**

Append to `bins/agent/tests/reconcile.rs`, in the binding section (reuse `binding_route()`, `ready_binding()` and the surrounding `ctx()` helper — copy the nearest existing binding test's fixture, do not invent one):

```rust
/// The owner's ceiling is projected into their namespace on every binding pass, so a raise takes
/// effect without a roll and a namespace made before quotas existed gets one on its next reconcile.
#[tokio::test]
async fn a_binding_pass_writes_the_owners_resource_quota() {
    let quota = Route {
        method: "GET",
        path: "/apis/rustic-git.io/v1alpha1/quotas/alice".into(),
        status: 200,
        body: serde_json::json!({
            "apiVersion": "rustic-git.io/v1alpha1", "kind": "Quota",
            "metadata": {"name": "alice"},
            "spec": {"workspaces": 5, "environments": 2, "snapshots": 20, "diskGb": 100, "cpu": 12, "memoryGb": 48}
        }),
    };
    let (ctx, rec) = /* the same ctx builder the neighbouring binding test uses */ binding_ctx(vec![quota]);
    rustic_git_agent::binding::apply_binding(&binding_obj("alice"), &ctx).await.unwrap();

    let sent = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/resourcequotas/owner-quota");
    assert!(!sent.is_empty(), "{:?}", rec.calls());
    assert_eq!(sent[0]["spec"]["hard"]["limits.cpu"], "12");
    assert_eq!(sent[0]["spec"]["hard"]["limits.memory"], "48Gi");
}
```

Adjust `binding_ctx`/`binding_obj` to the helper names the file already has (read `bins/agent/tests/reconcile.rs:386-500`); the assertions are the point. The apply path is server-side apply, so the recorded call is a `PATCH` on the named object — confirm the exact path from `rec.calls()` on the first red run and pin that.

- [ ] **Step 5: Run to verify it fails**

Run: `cargo test -p rustic-git-agent-bin --test reconcile -- --test-threads=1 resource_quota; echo exit=$?`
Expected: FAIL — no such call recorded.

- [ ] **Step 6: Write into both reconcilers**

In `bins/agent/src/binding.rs`, inside `apply_binding`'s `for team in teams_in_use(...)` loop, right after the `LimitRange` ensure:

```rust
        // The owner's ceiling, projected. A TEAM namespace gets the TEAM's quota: the working
        // copies in it are the team's, so the team's number is the one that bounds them.
        let q = rustic_git_workspaces::quota::effective(
            &ctx.client,
            if team.is_empty() { owner } else { &team },
            !team.is_empty(),
        )
        .await?;
        ensure(
            &Api::<k8s_openapi::api::core::v1::ResourceQuota>::namespaced(ctx.client.clone(), &ns),
            &k8s::resource_quota(&ns, owner, "workspace", &q),
            ctx,
        )
        .await?;
```

Add `ResourceQuota` to the `k8s_openapi::api::core::v1` import at the top of the file rather than naming it inline, matching the file's style.

In `bins/agent/src/controller/environment.rs`, right after the `LimitRange` ensure (around `:373`):

```rust
    // The same ceiling, in the environment's own namespace: an environment's services are its
    // owner's capacity too, and the namespace is where Kubernetes can enforce it.
    let q = rustic_git_workspaces::quota::effective(&ctx.client, &e.spec.owner, false).await?;
    ensure(
        &Api::<ResourceQuota>::namespaced(ctx.client.clone(), ns),
        &k8s::resource_quota(ns, &e.spec.owner, "environment", &q),
        ctx,
    )
    .await?;
```

The `false` here is deliberate and needs the comment: an environment's `spec.owner` may be a team slug, and the agent cannot ask the directory which it is — so the FALLBACK it picks when no `Quota` object exists is the user table, which is the smaller of the two. A team that has ever had a request approved has an object of its own and gets the right number. Write that as:

```rust
    // `false`: the agent has no directory to ask whether this slug is a team, so an owner with no
    // `Quota` object of their own falls back to the SMALLER table. A team whose quota has ever
    // been set has an object, and that object is what is read — the fallback is the only case that
    // guesses, and it guesses conservatively.
```

`ReconcileErr` must absorb a `kube::Error` from `effective` — it already does (`?` on `api.list` throughout `binding.rs`); if the `From` impl is missing for this path, map with `ReconcileErr::from`.

- [ ] **Step 7: Run the tests and gates**

```bash
cargo test -p rustic-git-workspaces -p rustic-git-agent-bin -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
```
Expected: PASS. Other binding tests will need the `quotas/{owner}` route added to their mock route list — a `not_found(...)` route is the right answer there, which exercises the fallback.

- [ ] **Step 8: Commit**

```bash
git add crates/workspaces/src/k8s.rs bins/agent/src/binding.rs bins/agent/src/controller/environment.rs bins/agent/tests/reconcile.rs
git commit -m "Project the owner's quota into their namespaces as a ResourceQuota"
```

---

### Task 7a: Web — the usage bar and the request form

**Files:**
- Create: `web/apps/web/src/lib/quota.ts`, `web/apps/web/src/lib/quota.test.ts`
- Modify: `web/apps/web/src/lib/api.ts`
- Create: `web/apps/web/src/components/app/quota-bar.tsx`, `web/apps/web/src/components/app/quota-request-dialog.tsx`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/page.tsx`
- Modify: the workspace/environment create action that surfaces the 409 (`rg -n "createWorkspace" web/apps/web/src`)

**Interfaces:**
- Consumes: `GET /v1/quota`, `POST /v1/quota-requests` (Tasks 2 and 4).
- Produces:
  - `lib/quota.ts`: `type QuotaDim = "workspaces" | "environments" | "snapshots" | "diskGb" | "cpu" | "memoryGb"`; `type QuotaReport = { owner: string; limit: Record<QuotaDim, number>; used: Record<QuotaDim, number> }`; `DIMS: readonly QuotaDim[]`; `dimLabel(d: QuotaDim): string`; `percent(used: number, limit: number): number`; `atLimit(r: QuotaReport, d: QuotaDim): boolean`; `dimFromRefusal(message: string): QuotaDim | null`.
  - `lib/api.ts`: `getQuota(owner, token)`, `createQuotaRequest(body, token)`, `listQuotaRequests(params, token)`, `decideQuotaRequest(id, decision, note, token)`, `listSuperadmins(token)`.
  - `components/app/quota-bar.tsx`: `<QuotaBar report={…} />`.

- [ ] **Step 1: Write the failing test**

Create `web/apps/web/src/lib/quota.test.ts`:

```ts
import { describe, expect, test } from "bun:test";
import { atLimit, dimFromRefusal, dimLabel, percent, type QuotaReport } from "@/lib/quota";

const report = (used: number, limit: number): QuotaReport => ({
  owner: "karthik",
  limit: { workspaces: limit, environments: 2, snapshots: 20, diskGb: 100, cpu: 8, memoryGb: 32 },
  used: { workspaces: used, environments: 0, snapshots: 0, diskGb: 0, cpu: 0, memoryGb: 0 },
});

describe("percent", () => {
  test("is a whole percentage of the limit", () => {
    expect(percent(1, 4)).toBe(25);
    expect(percent(5, 5)).toBe(100);
  });
  // A limit of zero is a dimension nobody may use; the bar must read full, not divide by zero.
  test("a zero limit is full, never NaN", () => {
    expect(percent(0, 0)).toBe(100);
  });
  // Over-quota is possible: /v1 is read-then-write, and a limit can be lowered under existing use.
  test("clamps above the limit rather than overflowing the track", () => {
    expect(percent(7, 5)).toBe(100);
  });
});

test("atLimit is true only when there is no room left", () => {
  expect(atLimit(report(4, 5), "workspaces")).toBe(false);
  expect(atLimit(report(5, 5), "workspaces")).toBe(true);
  expect(atLimit(report(6, 5), "workspaces")).toBe(true);
});

// The 409's sentence is the contract between the api and this form: the dimension it names is the
// field the dialog pre-fills.
test("the refusal sentence names the dimension to ask about", () => {
  expect(dimFromRefusal("workspaces: 5 of 5 in use; request more under Quota")).toBe("workspaces");
  expect(dimFromRefusal("diskGb: 96 of 100 in use; request more under Quota")).toBe("diskGb");
  expect(dimFromRefusal("a workspace named \"x\" already exists here")).toBe(null);
});

test("every dimension has a label", () => {
  expect(dimLabel("diskGb")).toBe("Disk");
  expect(dimLabel("memoryGb")).toBe("Memory");
});
```

- [ ] **Step 2: Run to verify it fails**

Run (from `web/`): `bun test apps/web/src/lib/quota.test.ts; echo exit=$?`
Expected: FAIL — `@/lib/quota` does not exist.

- [ ] **Step 3: Write `lib/quota.ts`**

```ts
/** The six dimensions a quota has, in the order the bar shows them. The words are the api's own
 *  field names, so a 409 naming one is directly a key here — one vocabulary, not two. */
export const DIMS = ["workspaces", "environments", "snapshots", "diskGb", "cpu", "memoryGb"] as const;
export type QuotaDim = (typeof DIMS)[number];

export type QuotaReport = {
  owner: string;
  limit: Record<QuotaDim, number>;
  used: Record<QuotaDim, number>;
};

export function dimLabel(d: QuotaDim): string {
  return { workspaces: "Workspaces", environments: "Environments", snapshots: "Snapshots", diskGb: "Disk", cpu: "CPU", memoryGb: "Memory" }[d];
}

/** A whole percentage for the bar's width. A zero limit reads FULL rather than NaN — it is a
 *  dimension nobody may use — and over-quota clamps, because /v1 is read-then-write and a limit
 *  can be lowered under existing use. */
export function percent(used: number, limit: number): number {
  if (limit <= 0) return 100;
  return Math.min(100, Math.round((used / limit) * 100));
}

export function atLimit(r: QuotaReport, d: QuotaDim): boolean {
  return r.used[d] >= r.limit[d];
}

/** The dimension a 409 named, so the request form opens on the field that blocked them.
 *  The sentence is fixed by the api (`quota::refuse`); anything else is not a quota refusal. */
export function dimFromRefusal(message: string): QuotaDim | null {
  const word = message.split(":")[0]?.trim();
  return (DIMS as readonly string[]).includes(word) && message.includes("request more under Quota")
    ? (word as QuotaDim)
    : null;
}
```

- [ ] **Step 4: Run the test**

Run: `bun test apps/web/src/lib/quota.test.ts; echo exit=$?`
Expected: PASS.

- [ ] **Step 5: The api calls**

Append to `web/apps/web/src/lib/api.ts`, matching the file's existing `call<T>` style exactly:

```ts
/** An owner's ceiling and what is against it. Computed by the api on every request — there is no
 *  cached number to be stale. */
export function getQuota(owner: string, token: string) {
  return call<QuotaReport>(`/v1/quota?owner=${encodeURIComponent(owner)}`, { method: "GET", token });
}

export function createQuotaRequest(
  body: { owner?: string; requested: Partial<Record<QuotaDim, number>>; reason: string },
  token: string,
) {
  return call<QuotaRequestDoc>("/v1/quota-requests", { method: "POST", token, body: JSON.stringify(body) });
}

/** No `owner` and the superadmin claim is the whole queue; otherwise the caller's own and their
 *  teams'. */
export function listQuotaRequests(owner: string | undefined, token: string) {
  const q = owner ? `?owner=${encodeURIComponent(owner)}` : "";
  return call<QuotaRequestDoc[]>(`/v1/quota-requests${q}`, { method: "GET", token });
}

export function decideQuotaRequest(id: string, decision: "approve" | "deny", note: string, token: string) {
  return call<QuotaRequestDoc>(`/v1/quota-requests/${encodeURIComponent(id)}/${decision}`, {
    method: "POST",
    token,
    body: JSON.stringify({ note }),
  });
}

export type QuotaRequestDoc = {
  id: string;
  owner: string;
  requested: Partial<Record<QuotaDim, number>>;
  reason: string;
  state: "pending" | "approved" | "denied";
  decidedBy?: string | null;
  decidedAt?: string | null;
  note?: string | null;
  createdAt?: string | null;
};
```

with `import type { QuotaDim, QuotaReport } from "@/lib/quota";` at the top.

- [ ] **Step 6: The bar and the dialog**

`components/app/quota-bar.tsx` — a server component, six rows, tokens only (`--radius: 0`, no raw Tailwind colors):

```tsx
import { DIMS, atLimit, dimLabel, percent, type QuotaReport } from "@/lib/quota";

/** What the owner is using, against what they may. Six rows because six dimensions can each be the
 *  one that blocks a create, and a single "80% full" would hide which. */
export function QuotaBar({ report }: { report: QuotaReport }) {
  return (
    <div className="grid gap-2">
      {DIMS.map((d) => (
        <div key={d} className="grid grid-cols-[8rem_1fr_6rem] items-center gap-3 text-sm">
          <span className="text-muted-foreground">{dimLabel(d)}</span>
          <div className="h-2 bg-muted" role="presentation">
            <div
              className={atLimit(report, d) ? "h-2 bg-destructive" : "h-2 bg-primary"}
              style={{ width: `${percent(report.used[d], report.limit[d])}%` }}
            />
          </div>
          <span className="tabular-nums text-right">
            {report.used[d]} / {report.limit[d]}
          </span>
        </div>
      ))}
    </div>
  );
}
```

`components/app/quota-request-dialog.tsx` — copy the structure of an existing dialog with a server action (`components/app/new-token-dialog.tsx` is the closest sibling; reuse `useDialogUntilSuccess`). It takes `owner`, an optional `dim` to pre-select, renders one number input per dimension the user chooses to raise plus a required `reason` textarea, and calls a server action wrapping `createQuotaRequest`. Do not invent a new form pattern — mirror the sibling.

- [ ] **Step 7: Show them**

In `web/apps/web/src/app/(shell)/[owner]/(org)/page.tsx`, fetch the report with `requireToken` and render `<QuotaBar report={…} />` in a section titled "Quota", with the request dialog's trigger beside it. A `notFound`/`unavailable` result renders the section absent rather than failing the page — an owner page must not go blank because one call did.

In the workspace and environment create actions (`rg -n "createWorkspace\|createEnvironment" web/apps/web/src/app`), when the api answers `conflict` and `dimFromRefusal(message)` is non-null, return that message plus the dimension so the form shows the sentence with the request dialog's trigger beside it. The sentence is the api's; do not rewrite it in the UI.

- [ ] **Step 8: Run the web gates and commit**

```bash
cd web && bun run lint; echo exit=$?
bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
bun test; echo exit=$?
git add web/apps/web/src
git commit -m "Show quota use on the org page and offer a request from the refusal"
```

---

### Task 7b: Web — the `/admin` area

**Files:**
- Create: `web/apps/web/src/app/(shell)/admin/layout.tsx`, `page.tsx` (queue), `usage/page.tsx`, `defaults/page.tsx`, `regions/page.tsx`, `nodes/page.tsx`, `actions.ts`
- Modify: `web/apps/web/src/components/app/shell-nav.tsx` (`ROOT_PAGES`) and `shell-nav.test.ts`
- Modify: `web/apps/web/src/lib/session.ts` (expose the claim), `web/apps/web/src/auth.ts` (carry it)

**Interfaces:**
- Consumes: `listQuotaRequests`, `decideQuotaRequest`, `getQuota`, `listSuperadmins` (Task 7a); the `superadmin` JWT claim (Task 5a).
- Produces: `Session.user.superadmin: boolean`; `requireSuperadmin(): Promise<{session, token}>` in `lib/session.ts`.

- [ ] **Step 1: Write the failing test**

Append to `web/apps/web/src/components/app/shell-nav.test.ts`:

```ts
// `/admin` hangs off the root, not off a namespace: without this the chrome reads `admin` as an
// owner handle and shows the wrong crumb on every admin page. (`admin` is already a reserved
// handle server-side, so nobody's namespace can collide with it.)
test("the admin area is a root page, not an owner", () => {
  expect(place("/admin", "karthik")).toEqual({ kind: "org", owner: "karthik" });
  expect(place("/admin/usage", "karthik")).toEqual({ kind: "org", owner: "karthik" });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd web && bun test apps/web/src/components/app/shell-nav.test.ts; echo exit=$?`
Expected: FAIL — `place("/admin")` returns `{ owner: "admin" }`.

- [ ] **Step 3: Make `admin` a root page**

In `shell-nav.tsx`:

```ts
const ROOT_PAGES = ["settings", "new-repo", "new-team", "invite", "admin"];
```

Update the `ponytail:` comment above it to say `admin` is already refused as a handle (it is in `RESERVED` in `crates/pulls/src/directory/mod.rs`), so it does not widen the shadowing ceiling that comment names.

- [ ] **Step 4: Carry the claim into the session**

In `web/apps/web/src/auth.ts`, in the `jwt` callback, after the sign-in mint, decode the claim off the api token and stash it — the api token is a JWS whose payload is readable without the key, and the web only needs it to decide what to RENDER (every actual permission is the api's):

```ts
        // Read from the api token's own payload: the api decided it at sign-in, and the web must
        // never decide it. This gates what is SHOWN; every admin action is re-authorized by /v1
        // against the same claim, so a tampered cookie reveals a page and grants nothing.
        token.superadmin = readSuperadmin(r.value.token ?? "");
```

with a small helper in the same file:

```ts
function readSuperadmin(jws: string): boolean {
  try {
    const payload = jws.split(".")[1];
    return JSON.parse(Buffer.from(payload, "base64url").toString()).superadmin === true;
  } catch {
    return false;
  }
}
```

In the `session` callback: `session.user.superadmin = token.superadmin === true;` and add `superadmin?: boolean` to the Auth.js module augmentation beside `username`.

In `lib/session.ts`, add `superadmin: boolean` to `Session["user"]` (from `user.superadmin ?? false`), and:

```ts
/** Identity, a token, and the admin claim — or 404 for anyone without it.
 *
 *  Still not an access decision: /v1 re-checks the claim on every call this area makes. This only
 *  decides whether the page exists for this person, and 404 rather than 403 because whether an
 *  admin area is here is not a non-admin's to learn. */
export async function requireSuperadmin(next: string) {
  const { session, token } = await requireToken(next);
  if (!session.user.superadmin) notFound();
  return { session, token };
}
```

- [ ] **Step 5: The pages**

`app/(shell)/admin/layout.tsx` — calls `requireSuperadmin("/admin")`, renders a tab row (Queue / Usage / Defaults / Regions / Nodes) with `NavTabs`, exactly as `[owner]/(org)/layout.tsx` does.

`app/(shell)/admin/page.tsx` — the queue: `listQuotaRequests(undefined, token)`, filtered to `state === "pending"` first and the last few decided ones below, each row showing owner, the dimensions asked for, the reason and the age (`lib/time.ts` for the age — do not format a date by hand). Two buttons per pending row calling a server action in `admin/actions.ts` that wraps `decideQuotaRequest`, with a note field. Copy the destructive-action pattern from a repo `settings/` page rather than inventing one.

`admin/usage/page.tsx` — every owner's usage against their limit. There is no "list every owner" call, so the owners come from the requests and quotas the api already returns: list every `QuotaRequest` for the owner names, `getQuota` per owner. Mark it:

```tsx
{/* ponytail: the owner list is derived from who has ever had a Quota or a request; an owner who
    has neither is not shown. A `GET /v1/quota/all` is the upgrade when the list has to be
    complete. */}
```

`admin/defaults/page.tsx` — the two `default-user` / `default-team` objects, shown and editable. Editing them is a `POST /v1/quota-requests` + immediate approve, which is a hack; instead, add the one route this page needs to `/v1` in this task: `PUT /v1/quota/{owner}` taking a `QuotaSpec`, `require_admin`-gated, applying the same create-or-patch `approve_quota_request` uses. Factor that body out of Task 5b's handler into `async fn write_quota(s, owner, spec)` and call it from both — one writer, so the two can never disagree about how a quota lands.

`admin/regions/page.tsx` — the existing region list and the create form, moved under the claim (the route is already admin-gated as of Task 5b).

`admin/nodes/page.tsx` — each node's `rustic-git.io/decommission-status` annotation. There is no `/v1` route for nodes today; add `GET /v1/nodes` to `crates/workspaces/src/api.rs`, `require_admin`-gated, returning `[{name, ready, decommission, decommissionStatus}]` read from the `Node` list. `deploy/k3s/api-rbac.yaml` gains:

```yaml
  # Read-only, for the admin area's decommission view: `rustic-git.io/decommission-status` is the
  # annotation an operator watches a drain through, and it is on the Node.
  - apiGroups: [""]
    resources: ["nodes"]
    verbs: ["get", "list"]
```

- [ ] **Step 6: Run the gates and commit**

```bash
cargo test -p rustic-git-workspaces -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
cd web && bun run lint; echo exit=$?
bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
bun test; echo exit=$?
```

```bash
git add web/apps/web/src crates/workspaces/src/api.rs deploy/k3s/api-rbac.yaml
git commit -m "Add an admin area for the quota queue, usage, defaults, regions and nodes"
```

---

### Task 8: Docs and the end-to-end round trip

**Files:**
- Modify: `CLAUDE.md` — the "Workspaces and environments" section
- Modify: `deploy/k3s/README.md` — a release note with the apply order
- Modify: `tests/ws_e2e.sh` — the claim in `mint_jwt`, and the limit → request → approve → succeed block

- [ ] **Step 1: `CLAUDE.md`**

Add to the "Workspaces and environments" section, after the paragraph about the CRDs being the source of truth:

```markdown
**Allocation is bounded by a `Quota` per owner.** A cluster-scoped `Quota` CR named by the owner
slug (a person or a team) caps six dimensions — workspaces, environments, snapshots, diskGb, cpu,
memoryGb — with `default-user`/`default-team` as the fallback for an owner who has none and a
compiled-in table (`crd::default_quota`) behind those, so a missing object is never "unlimited".
**Usage is computed from the CRDs on every request and never cached** (`crates/workspaces/src/quota.rs`):
a stored counter can only be wrong in the direction that hands out allocation nobody has. `/v1`
refuses an over-quota create, restore, clone or push with `409` and one sentence — `"{dimension}:
{used} of {limit} in use; request more under Quota"` — from `quota::refuse`, through the single
gate `guard_alloc`; the check is read-then-write, so two concurrent creates can overshoot by one
and the agent's per-namespace `ResourceQuota` (written on every `OwnerBinding` and environment
reconcile, from the same effective `Quota`) is the hard stop for cpu and memory. A raise is a
`QuotaRequest` CR: the owner, or a team member whose directory role is at least admin, opens ONE
pending request at a time; only a superadmin approves, which writes the `Quota` **before** marking
the request. **Superadmin is a claim, not an owner** — `superadmin: true` in the session JWT,
minted at sign-in from a `superadmins` collection in the directory that
`RUSTIC_GIT_WORKSPACES_ADMINS` merely bootstraps; `/v1`'s `require_admin` and `may_act_on`'s third
arm read it (and log every cross-owner access), the web's `/admin` area is gated on it, and it
never changes who owns anything.
```

- [ ] **Step 2: `deploy/k3s/README.md`**

Add a release note under "Applying", in the same shape as the existing per-file entries:

```markdown
### Release: quotas and superadmin

Apply in this order — the CRDs first, or `/v1` and the agent both 403 on a kind the cluster does
not have; then the two roles, or the same calls 403 on the verbs.

1. `kubectl apply -f crds.yaml` — adds `quotas` and `quotarequests`. Additive; existing objects are
   untouched.
2. `kubectl apply -f api-rbac.yaml -f agent-rbac.yaml` — the api gains `quotas` read/create/patch,
   `quotarequests` create + `/status`, and `nodes` read for the admin area's decommission view; the
   agent gains `quotas` read and `resourcequotas` create/patch. `agent-admission.yaml` is
   **unchanged**: the agent writes no `Quota` spec, so there is no new spec write to refuse.
3. Repin and roll the api, server and agent images (`deploy/pin.sh`, `deploy/roll.sh`).
4. Set `RUSTIC_GIT_WORKSPACES_ADMINS` on the api Deployment if it is not already set, and roll it
   once: the api seeds those addresses into the directory's `superadmins` collection at boot. After
   that first boot the env is only a bootstrap — the list is managed at
   `POST`/`DELETE /api/admin/superadmins/{user}`, and removing an address from the env revokes
   nobody.
5. Optionally create the two fallback objects; without them the compiled-in table applies, which is
   the same numbers:

   ```sh
   kubectl apply -f - <<'YAML'
   apiVersion: rustic-git.io/v1alpha1
   kind: Quota
   metadata: {name: default-user}
   spec: {workspaces: 5, environments: 2, snapshots: 20, diskGb: 100, cpu: 8, memoryGb: 32}
   ---
   apiVersion: rustic-git.io/v1alpha1
   kind: Quota
   metadata: {name: default-team}
   spec: {workspaces: 20, environments: 8, snapshots: 80, diskGb: 400, cpu: 32, memoryGb: 128}
   YAML
   ```

**Ordering matters the other way on a rollback:** roll the images back before removing the CRDs, or
every `/v1` create 500s on a kind that has gone.

**What existing owners see:** nothing changes until they cross a limit. Anyone already over one is
not touched — a quota blocks new allocation only, never an existing working copy.
```

- [ ] **Step 3: The e2e block**

In `tests/ws_e2e.sh`, extend `mint_jwt` with a fourth argument:

```sh
mint_jwt() {
  # Workspace/volume ownership keys on the USERNAME claim (vol/{owner}/... paths validate it
  # as an owner name; an email's @/. can never route), so every minted token carries one.
  # The fourth argument is the platform-administrator claim, which /v1's admin surfaces (regions,
  # quota decisions) read — it is omitted when false, exactly as `Jwt::mint` omits it.
  local email="$1" name="$2" username="$3" admin="${4:-}"
  local now exp header payload signing_input sig extra=""
  [ -n "$admin" ] && extra=',"superadmin":true'
  now=$(date +%s)
  exp=$((now + 43200))
  header=$(printf '{"typ":"JWT","alg":"HS256"}' | b64url)
  payload=$(printf '{"sub":"%s","name":"%s","username":"%s","typ":"session"%s,"iat":%d,"exp":%d}' "$email" "$name" "$username" "$extra" "$now" "$exp" | b64url)
  signing_input="$header.$payload"
  sig=$(printf '%s' "$signing_input" | openssl dgst -sha256 -hmac "$JWT_SECRET" -binary | b64url)
  echo "$signing_input.$sig"
}
```

and mint the admin with it: `ADMIN_TOKEN=$(mint_jwt "$ADMIN_EMAIL" "E2E Admin" "e2eadmin" admin)`.

Then, near the end of the script (after the workspace round trip, before the summary line), add:

```sh
# ---------------------------------------------------------------------------
# Quotas: hit a limit, ask for more, approve as a superadmin, succeed.
#
# The whole loop against the real API server, because the parts that can only break together are
# the ones a unit test cannot reach: the CRD is installed, the api's RBAC covers the write, and the
# approve path's Quota lands before the request is marked.
# ---------------------------------------------------------------------------
log "lowering the user's workspace quota to what they already have"
CURRENT_WS=$(curl -fsS "$BASE/v1/quota" -H "Authorization: Bearer $USER_TOKEN" | field used.workspaces)
[ -n "$CURRENT_WS" ] || fail "GET /v1/quota returned no used.workspaces"
kubectl apply -f - <<YAML || fail "could not write the test Quota"
apiVersion: rustic-git.io/v1alpha1
kind: Quota
metadata: {name: $USER_NAME}
spec: {workspaces: $CURRENT_WS, environments: 8, snapshots: 80, diskGb: 400, cpu: 32, memoryGb: 128}
YAML

log "checking a create at the limit is refused with the quota sentence"
CODE=$(curl -s -o /tmp/ws-e2e-quota.txt -w '%{http_code}' -X POST "$BASE/v1/workspaces" \
  -H "Authorization: Bearer $USER_TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"e2e-over","region":"'"$REGION_ID"'","quota_gb":5,"image":"'"$WS_IMAGE"'"}')
[ "$CODE" = "409" ] || fail "a create at the workspace limit must 409, got $CODE"
grep -q "request more under Quota" /tmp/ws-e2e-quota.txt \
  || fail "the refusal must name the quota: $(cat /tmp/ws-e2e-quota.txt)"

log "opening a quota request"
REQ_ID=$(curl -fsS -X POST "$BASE/v1/quota-requests" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"requested":{"workspaces":'"$((CURRENT_WS + 2))"'},"reason":"e2e"}' | field id)
[ -n "$REQ_ID" ] || fail "no id from POST /v1/quota-requests"

log "checking a second pending request is refused"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/quota-requests" \
  -H "Authorization: Bearer $USER_TOKEN" -H 'Content-Type: application/json' \
  -d '{"requested":{"workspaces":99},"reason":"again"}')
[ "$CODE" = "409" ] || fail "a second pending request must 409, got $CODE"

log "checking the owner cannot approve their own request"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/quota-requests/$REQ_ID/approve" \
  -H "Authorization: Bearer $USER_TOKEN" -H 'Content-Type: application/json' -d '{}')
[ "$CODE" = "403" ] || fail "an owner approving their own request must 403, got $CODE"

log "approving as a superadmin"
curl -fsS -X POST "$BASE/v1/quota-requests/$REQ_ID/approve" -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"note":"e2e"}' >/dev/null || fail "approve failed"
STATE=$(kubectl get quotarequest "$REQ_ID" -o jsonpath='{.status.state}')
[ "$STATE" = "approved" ] || fail "request state is $STATE, expected approved"
GRANTED=$(kubectl get quota "$USER_NAME" -o jsonpath='{.spec.workspaces}')
[ "$GRANTED" = "$((CURRENT_WS + 2))" ] || fail "quota is $GRANTED, expected $((CURRENT_WS + 2))"
# The five dimensions the request did not name are untouched: approving raises what was asked for,
# never resets what somebody already granted.
[ "$(kubectl get quota "$USER_NAME" -o jsonpath='{.spec.diskGb}')" = "400" ] \
  || fail "approving must not reset the dimensions the request did not name"

log "checking the same create now succeeds"
OVER_ID=$(curl -fsS -X POST "$BASE/v1/workspaces" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"e2e-over","region":"'"$REGION_ID"'","quota_gb":5,"image":"'"$WS_IMAGE"'"}' | field id)
[ -n "$OVER_ID" ] || fail "the create after the approval did not return an id"

log "checking the namespace carries the owner's ResourceQuota"
kubectl -n "$WS_NS" get resourcequota owner-quota >/dev/null \
  || fail "no owner-quota ResourceQuota in $WS_NS"
[ "$(kubectl -n "$WS_NS" get resourcequota owner-quota -o jsonpath='{.spec.hard.limits\.cpu}')" = "32" ] \
  || fail "the ResourceQuota does not carry the owner's cpu ceiling"

curl -fsS -X DELETE "$BASE/v1/workspaces/$OVER_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null || true
```

Extend the final `echo "OK: ..."` summary with `, quota (limit refused, request, one-pending, approve, ResourceQuota, create succeeds)`.

- [ ] **Step 4: Check the script parses**

Run: `bash -n tests/ws_e2e.sh; echo exit=$?` and `shellcheck tests/ws_e2e.sh 2>/dev/null | head -20` (shellcheck is advisory here — match the file's existing style).
Expected: exit=0 from `bash -n`.

- [ ] **Step 5: Run every gate one last time**

```bash
cargo test -p rustic-git-workspaces -p rustic-git-agent-bin -- --test-threads=1; echo exit=$?
cargo test -p rustic-git-core -p rustic-git-api -p rustic-git-pulls -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml; cargo test -p rustic-git-workspaces --test crd_yaml; echo exit=$?
cd web && bun run lint; bunx tsc --noEmit -p apps/web/tsconfig.json; bun test; echo exit=$?
```
Expected: PASS everywhere, and `git status` shows `deploy/k3s/crds.yaml` unchanged by the regen (it was committed in Task 1).

- [ ] **Step 6: Commit**

```bash
git add CLAUDE.md deploy/k3s/README.md tests/ws_e2e.sh
git commit -m "Document quotas and superadmin and cover the request loop end to end"
```

---

## Self-Review

**Spec coverage.** §1 One quota per owner → Tasks 1 (CRD, defaults table), 2 (usage, `GET /v1/quota`). §2 Enforcement → Task 3 (`/v1`, all six live routes plus the note on the two that have no route), Task 6 (`ResourceQuota`). §3 Quota requests → Tasks 1 (CRD), 4 (create/list, the role rule, the one-pending rule), 5b (approve/deny, the decided-once rule). §4 Superadmin → Task 5a (directory list, bootstrap, JWT claim), 5b (`require_admin`, `may_act_on`'s third arm with the audit line, `/v1/regions` moved under it), 7b (`/admin`: queue, usage, defaults, regions, node decommission status). Rules → the Global Constraints block. Cases table → the recorder tests in Tasks 3, 4 and 5b, plus the e2e in Task 8. Testing → Tasks 2/3/4/5b (`/v1` recorder), 6 (agent recorder), 7a (`bun:test`), 8 (live).

**Placeholders.** None: every code step carries the code, every test step the assertions. The three places that say "copy the sibling" name the exact sibling file and line range (`api_teams.rs:26-83`, `reconcile.rs:386-500`, `new-token-dialog.tsx`), because the harnesses are long and duplicating them here would be the drift.

**Type consistency.** `QuotaSpec` field names are `workspaces/environments/snapshots/disk_gb/cpu/memory_gb` throughout, serialized camelCase, and `Dim::word` returns exactly those camelCase words — which is also `QuotaDim` in `lib/quota.ts`, so the 409 sentence, the CRD field and the web key are one vocabulary. `quota::check`, `quota::refuse` and `guard_alloc` are the only producers of the sentence. `Caller { name, superadmin }` is introduced in 5b and used by name in Tasks 2, 4 and 5b, each with the pre-5b fallback spelled out. `TeamRole`'s ordering is the rank rule and there is no `rank()` anywhere.
