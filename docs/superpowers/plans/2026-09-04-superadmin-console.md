# Superadmin console Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the operator one console (`/superadmin`) that answers "what needs a decision", "is
it healthy", "what does this owner have", "run a change safely" and "what happened", instead of
the flat Requests/Usage/Quotas/Clusters/Monitoring tabs the quotas-and-superadmin plan shipped.
Everything the operator can do is audited forever.

**Architecture:** No new CRDs. The admin server process (`bins/api` with `RUSTIC_GIT_API_ROLE=admin`,
router `crates/workspaces/src/api/admin.rs` + `admin/{settings,schema}.rs`) grows: an append-only
audit log written straight to the object store it already has a handle to (`ApiState::keys`,
`s.keys.as_ref().map(|store| store.os.clone())`, the same accessor `admin/settings.rs`'s
`object_store()` uses) at `audit/{yyyy-mm}/{ts}-{ulid}.json`, one composed detail/list endpoint per
area (`/admin/owners`, `/admin/owners/{slug}`, `/admin/clusters`, `/admin/clusters/{region}`,
`/admin/monitoring/signals`, `/admin/overview`), and drain/undrain/decommission routes that patch
the `Node` object's `rustic-git.io/decommission` label the way `bins/agent/src/decommission.rs`
already reads it. Every write route funnels through one `audit::record` call so no route can land
silently. The server-tier superadmin management routes (`crates/api/src/teams.rs`,
`/api/admin/superadmins/{user}`) gain the "not yourself / not the last one" rules; nothing about
where they live changes. The web area (`web/apps/web/src/app/(shell)/superadmin/*`) is
restructured from five flat tabs into the spec's eight areas, with a left rail on desktop
(`shell-nav.tsx`'s `SUPERADMIN_TABS` becomes a rail component) and the existing tab row on narrow
screens, `place()`'s `superadmin` kind unchanged.

**Tech Stack:** Rust (`axum`, `kube`/`k8s-openapi`, `serde`, `slatedb::object_store`, `chrono`,
`ulid`), Kubernetes CRDs (none new) + RBAC (`nodes: patch` added), Next.js app router +
`bun:test`.

**Spec:** `docs/superpowers/specs/2026-09-04-superadmin-console-design.md` — binding. Decisions
recorded there (approve-with-edits, drain AND decommission in phase 1, audit kept forever) are
final for this plan; read it before Task 1.

## Global Constraints

These apply to every task; they are not repeated per task.

- **No tool attribution anywhere.** Commit subjects are imperative sentence case, no
  `Co-Authored-By`, no `Generated with`, no Claude reference in code comments or docs.
- **Comments say WHY, never what.** Match the density of `bins/server/src/router/route.rs`.
- **Keep every `// ponytail:` marker** you edit near, and add one for any deliberate shortcut with
  a named ceiling and upgrade path.
- **Vocabulary, verbatim from the spec:** workspace, environment, push, snapshot, restore, clone,
  delete; owner, person, team, region, node, workload, roll, drain. Never "fork", never "commit"
  for a snapshot, never "job" or "queue", never "decommission" where the spec says "drain" (they
  are two different console actions — see Task 4).
- **Decisions first.** The landing view (`/superadmin`, Task 7) is what needs attention, never a
  menu.
- **Every page answers one question, has one primary action**, and shows the facts that action
  needs without a second click.
- **Dangerous is loud, routine is quiet.** One confirmation for any write; a SECOND, naming the
  consequence, for: the server StatefulSet roll (database ownership moves), a node drain (running
  work keeps running but nothing new lands there), a node decommission (the VM may now be
  deleted), removing a superadmin, deactivating a region.
- **Everything is auditable, forever.** Every write records who, when, what, why and the result;
  nothing writes silently; retention is "keep everything" per the Decisions section — no pruning
  route, no TTL, no size cap in this plan.
- **Truth from the cluster.** Every number is computed from CRDs and pods on the request path,
  never a cached counter.
- **Same chrome as the product.** Tokens, `--radius: 0`, sibling components; denser, not
  different.
- **Reason on every write except approve** (the request already carries its reason). Deny, set
  quota, roll, drain, decommission, add/remove superadmin: a required, non-empty note, stored on
  the audit row.
- **Freshness:** pages poll every 10 s while open (the app's existing auto-refresh pattern), 2 s
  while a roll or drain the page itself started is in progress.
- **Errors:** 409 shows the conflicting state; 422 shows the field and range (`range_err`'s
  shape, `crates/core/src/settings.rs`); 5xx shows a retry with the request id.
- **Gates — run all of these before every commit, unpiped:**

  ```bash
  cargo test -p rustic-git-workspaces -p rustic-git-api -p rustic-git-agent-bin -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```

  Plus, in any web task, from `web/`: `bun run lint`, `bunx tsc --noEmit -p apps/web/tsconfig.json`,
  `bun test` (all three, unpiped, `echo exit=$?` after each).

  Note on clippy: `--all-targets` has pre-existing lints in test targets. The bar is **no new
  warnings in files you touch**; if the run is red, read the paths and confirm none is a file this
  task edited.

## File Structure

| File | Create/Modify | Responsibility |
|---|---|---|
| `crates/workspaces/src/audit.rs` | Create | `record()`, `AuditEntry`, object-store key shape, list/filter/CSV |
| `crates/workspaces/src/lib.rs` | Modify | `pub mod audit;` |
| `crates/workspaces/src/api/admin.rs` | Modify | Call `audit::record` from every existing write handler; new routes list |
| `crates/workspaces/src/api/admin/owners.rs` | Create | `GET /admin/owners`, `GET /admin/owners/{slug}` |
| `crates/workspaces/src/api/admin/clusters.rs` | Create | `GET/POST /admin/clusters*`, drain/undrain/decommission |
| `crates/workspaces/src/api/admin/monitoring.rs` | Create | `GET /admin/monitoring/signals`, restarts |
| `crates/workspaces/src/api/admin/overview.rs` | Create | `GET /admin/overview` |
| `crates/workspaces/src/api/admin/audit.rs` | Create | `GET /admin/audit`, CSV export |
| `crates/workspaces/src/api/workloads.rs` | Modify | Nothing structural; `restarts_last_hour` helper for Monitoring |
| `deploy/k3s/api-rbac.yaml` | Modify | `nodes: get,list,patch` (label-scoped in the handler) |
| `crates/api/src/teams.rs` | Modify | `add_superadmin`/`remove_superadmin` gain the two rules |
| `crates/api/src/teams.rs` (tests) | Modify | Rule tests |
| `web/apps/web/src/lib/api.ts` | Modify | Typed calls for every new/changed route |
| `web/apps/web/src/lib/audit.ts` (+ `.test.ts`) | Create | Pure CSV-row and filter-query formatting |
| `web/apps/web/src/components/app/shell-nav.tsx` | Modify | `SUPERADMIN_TABS` → the eight areas, rail on desktop |
| `web/apps/web/src/app/(shell)/superadmin/rail.tsx` | Create | Left-rail / tab-row component |
| `web/apps/web/src/app/(shell)/superadmin/page.tsx` | Modify | Overview |
| `web/apps/web/src/app/(shell)/superadmin/requests/page.tsx` | Create | Requests (moved from `page.tsx`) |
| `web/apps/web/src/app/(shell)/superadmin/requests/decision-panel.tsx` | Create | Editable approve panel |
| `web/apps/web/src/app/(shell)/superadmin/owners/page.tsx` | Create | Owners list + Defaults card |
| `web/apps/web/src/app/(shell)/superadmin/owners/[slug]/page.tsx` | Create | Owner detail |
| `web/apps/web/src/app/(shell)/superadmin/clusters/page.tsx` | Modify | List (region cards) |
| `web/apps/web/src/app/(shell)/superadmin/clusters/[region]/page.tsx` | Create | Detail: nodes, drain/undrain/decommission, workloads |
| `web/apps/web/src/app/(shell)/superadmin/monitoring/page.tsx` | Modify | Add Signals table |
| `web/apps/web/src/app/(shell)/superadmin/audit/page.tsx` | Create | Filters, CSV export |
| `web/apps/web/src/app/(shell)/superadmin/access/page.tsx` | Create | Superadmin list, add/remove |
| `web/apps/web/src/app/(shell)/superadmin/configuration/page.tsx` | Create | Read-only schema render |
| `web/apps/web/src/app/(shell)/superadmin/actions.ts` | Modify | New server actions: drain, decommission, owners set-quota, superadmin add/remove |
| `web/apps/web/src/app/(shell)/superadmin/usage/page.tsx`, `quotas/page.tsx` | Delete | Folded into Owners |
| `CLAUDE.md` | Modify | Superadmin paragraph |
| `deploy/k3s/README.md` | Modify | Release note: apply `api-rbac.yaml` for the `nodes: patch` verb |
| `tests/ws_e2e.sh` | Modify | Drain round trip via `$ADMIN_BASE`, audit row present |

---

### Task 1: The audit log — writer + `GET /admin/audit`

**Files:**
- Create: `crates/workspaces/src/audit.rs`
- Modify: `crates/workspaces/src/lib.rs` (`pub mod audit;`)
- Create: `crates/workspaces/src/api/admin/audit.rs`
- Modify: `crates/workspaces/src/api/admin.rs` (mount `/admin/audit`, wire the writer into every
  existing write handler)
- Test: `crates/workspaces/src/audit.rs` (unit), `crates/workspaces/tests/api_admin_audit.rs`
  (integration, new)

**Interfaces:**
- Produces: `audit::AuditEntry { ts: String, actor: String, action: String, target: String,
  reason: Option<String>, result: &'static str }` (`result` is `"ok"` or `"error:<code>"`);
  `audit::record(os: &Arc<dyn ObjectStore>, entry: &AuditEntry) -> Result<(), object_store::Error>`;
  `audit::list(os, filter: AuditFilter, cursor: Option<String>, limit: usize) -> Result<AuditPage,
  object_store::Error>` where `AuditFilter { actor: Option<String>, action: Option<String>,
  target: Option<String>, from: Option<String>, to: Option<String> }`.
- Consumes: `ApiState::keys` (already exists), `admin::caller()` (already exists, gives the
  actor's name).

- [ ] **Step 1: Write the failing tests**

`crates/workspaces/src/audit.rs`, test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The key shape is the contract with everything downstream: `{yyyy-mm}/{ts}-{ulid}.json`
    /// under `audit/`, lexicographically sortable within a month because `ts` is RFC 3339.
    #[test]
    fn the_object_key_sorts_by_time_within_a_month() {
        let a = object_key("2026-09-04T10:00:00Z", "01J...A");
        let b = object_key("2026-09-04T11:00:00Z", "01J...B");
        assert!(a.starts_with("audit/2026-09/"));
        assert!(a < b);
    }

    /// Every field the spec's Audit page needs, round-tripped through JSON exactly as it will be
    /// read back by `list`.
    #[test]
    fn an_entry_round_trips() {
        let e = AuditEntry {
            ts: "2026-09-04T10:00:00Z".into(),
            actor: "op@example.com".into(),
            action: "deny".into(),
            target: "acme".into(),
            reason: Some("over budget".into()),
            result: "ok",
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: AuditEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.actor, "op@example.com");
        assert_eq!(back.result, "ok");
    }
}
```

`crates/workspaces/tests/api_admin_audit.rs` (new file, mirrors the harness other
`tests/api_admin_*.rs` files use — `rg -n "fn admin_state\|fn test_router" crates/workspaces/tests/api_admin_settings.rs`
for the exact setup helpers to reuse):

```rust
/// A write route with no reason still lands a row (approve is exempt; deny is not) — the row is
/// what the Audit page and Task 7's Overview both read, so a route that forgets to call
/// `audit::record` is invisible to both, forever, not just until the next poll.
#[tokio::test]
async fn deny_quota_request_writes_an_audit_row() { /* create a QuotaRequest, POST deny with a
    note, then GET /admin/audit and assert one row: action "deny", target the owner, reason the
    note, result "ok" */ }

/// Filtering by actor, action and target each narrow the result independently.
#[tokio::test]
async fn audit_list_filters_by_actor_action_and_target() { /* write three rows with
    audit::record directly (bypassing the HTTP layer, since only the object store matters here),
    then assert each of ?actor=, ?action=, ?target= returns only the matching row */ }
```

- [ ] **Step 2: `audit::record`/`audit::list`/`object_key`**

  Object-store access follows `admin/settings.rs`'s `object_store(s)` (`s.keys.as_ref().map(|store|
  store.os.clone())`); listing follows `crates/registry/src/lib.rs`'s `list_dir_names` shape
  (`list_with_delimiter`, `os.list(Some(&prefix))` for the flat case since entries are files, not
  directories). `record` does one `os.put` per call — no batching, no queue: a lost audit write on
  a transient object-store error is a real gap, so `record`'s caller (Step 3) logs the error via
  `tracing::error!` but never fails the write it is auditing (the write already happened; refusing
  the response because the audit log had one bad `put` would make the platform's actual safety net
  less reliable, not more). `list` reads the requested month prefixes (`from`/`to` narrow which
  `audit/{yyyy-mm}/` prefixes are walked; unfiltered defaults to the last 3 months, newest first,
  paged by `limit` with a cursor that is just the last key read) and filters `actor`/`action`/`target`
  in memory after fetching — ponytail: no per-field index, a full-month scan per query; add one if
  a fleet's monthly audit volume ever makes this slow, not ahead of evidence it does.

- [ ] **Step 3: Wire every existing write handler**

  In `crates/workspaces/src/api/admin.rs`, `admin/settings.rs`, `admin/workloads` roll route: after
  every successful write, call `audit::record` with the caller's name (`caller(&s,
  &headers).await?.name`), the action word (`"approve"`, `"deny"`, `"set-quota"`, `"roll"`,
  `"add-region"`, `"deactivate-region"`, `"put-central-settings"`, `"revert-central-settings"`,
  `"put-cluster-settings"`, `"revert-cluster-settings"`), the target (owner slug, workload
  `scope/name`, region id, or settings scope), the reason/note where the route has one, and
  `"ok"`. On a route that returns before writing (a validation 422, a 409), no row is written —
  the promise is "every write is audited", not "every request".

  Do this by adding one call at each existing write's success path rather than a middleware,
  because the action word and target differ per route and a generic wrapper would need to parse
  them back out of the response — worse than the caller just stating them, per the ladder (rung 2:
  nothing here to reuse, a wrapper would be new complexity for the same line count).

- [ ] **Step 4: `GET /admin/audit`**

  `admin/audit.rs`: `?actor=&action=&target=&from=&to=&cursor=&limit=` (default `limit` 50, max
  200) → `{ rows: [AuditEntry], next_cursor: Option<String> }`. Mount at
  `/admin/audit` on the existing router in `admin.rs`, inside the same `route_layer` guard.

- [ ] **Step 5: `GET /admin/audit.csv`**

  Same filters, `text/csv` response, header row `ts,actor,action,target,reason,result`, values
  CSV-quoted (a reason containing a comma must round-trip in a spreadsheet). No pagination —
  bounded by the same `from`/`to` window, and a truly large export is an operator problem to solve
  with a narrower date range, not this route's to solve with streaming.

- [ ] **Step 6: Run tests, verify, commit**

  ```bash
  cargo test -p rustic-git-workspaces audit -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```

  Commit: "Add the superadmin audit log and every write's row"

---

### Task 2: Requests — decided-history filter and edited approve

**Files:**
- Modify: `crates/workspaces/src/api/admin.rs` (`list_all_quota_requests`, `approve_quota_request`)
- Test: `crates/workspaces/tests/api_admin.rs` (or wherever `list_all_quota_requests` is already
  tested — `rg -n "list_all_quota_requests\|approve_quota_request" crates/workspaces/tests`)

**Interfaces:**
- Modifies: `GET /admin/quota-requests` gains `?owner=&state=` (`state` one of
  `pending|approved|denied`, both filters AND'd, both optional, matching the existing
  `list_quota_requests` on `/v1` which already takes `?owner=`).
- Modifies: `POST /admin/quota-requests/{id}/approve` body gains an optional `requested:
  RequestedQuota` field alongside the existing `note` — when present it REPLACES `r.spec.requested`
  before `overlay` runs (the spec's "editable copy" — approve applies whatever was actually
  submitted, which is the original ask unless the operator edited it), so `overlay`'s signature is
  unchanged and only the caller decides which `RequestedQuota` it passes in.

- [ ] **Step 1: Write the failing tests**

```rust
/// `?owner=` and `?state=` each narrow the queue independently, and combine.
#[tokio::test]
async fn list_all_quota_requests_filters_by_owner_and_state() { /* three requests, two owners,
    mixed states; assert each filter and the AND of both */ }

/// Approving with an edited `requested` body grants what was submitted, not what was originally
/// asked — the "approve with edits" decision from the spec's Decisions section.
#[tokio::test]
async fn approve_with_an_edited_body_grants_the_edited_values() { /* request asks workspaces: 10;
    approve with requested: {workspaces: 6}; assert the resulting Quota.spec.workspaces == 6, not
    10, and the QuotaRequest.status.state == Approved (audit row too, per Task 1's wiring) */ }

/// Approving with no body (today's shape) is unchanged — the edit is optional, not mandatory.
#[tokio::test]
async fn approve_with_no_body_still_grants_exactly_what_was_asked() { /* unchanged existing
    behavior, guards the no-op default */ }
```

- [ ] **Step 2: Implement the query filter**

  `list_all_quota_requests`: add `Query(q): Query<ListFilter>` with `owner: Option<String>, state:
  Option<crd::RequestState>`, filter `rows` after `.list()` the same way `admin_list_ws` already
  narrows by owner — no new kube list-selector, since `QuotaRequest` carries no label to select on
  and the fleet-wide row count is small enough that filtering client-side (server-side of this
  process, client-side of the k3s API) is the honest lazy answer.

- [ ] **Step 3: Implement the edited-approve body**

  `Decision` struct (already holds `note: Option<String>`) gains `requested:
  Option<crd::RequestedQuota>`. In `approve_quota_request`, after parsing `note`: `let want =
  note.requested.clone().unwrap_or_else(|| r.spec.requested.clone());` then `overlay(base,
  &want)` in place of `overlay(base, &r.spec.requested)`.

- [ ] **Step 4: Run tests, verify, commit**

  ```bash
  cargo test -p rustic-git-workspaces api_admin -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```

  Commit: "Let a request queue be filtered and an approval be edited before it lands"

---

### Task 3: Owners — list and detail

**Files:**
- Create: `crates/workspaces/src/api/admin/owners.rs`
- Modify: `crates/workspaces/src/api/admin.rs` (mount, `write_quota_route` gains a required note)
- Test: `crates/workspaces/tests/api_admin_owners.rs`

**Interfaces:**
- Produces: `GET /admin/owners` → `Vec<OwnerRow>` (`OwnerRow { owner, is_team: bool, limit:
  crd::QuotaSpec, used: quota::Usage, source: "own"|"default", pending: bool }` — extends today's
  `usage_all`'s row shape with `source` and `pending`, reusing its owner-enumeration ponytail note
  verbatim); `GET /admin/owners/{slug}` → `OwnerDetail { owner, is_team, limit, used, source,
  workspaces: Vec<workspaces::WsRow>, environments: Vec<...>, volumes: Vec<VolumeRow>, requests:
  Vec<QuotaRequestDoc>, audit: Vec<audit::AuditEntry> }` (composes `quota::effective`,
  `quota::usage`, `super::workspaces::list_for_owner`, `super::environments::list_env`,
  a volumes listing (`rg -n "fn list_volumes\|struct VolumeRow" crates/workspaces/src/api` for the
  existing shape to reuse — Volumes already exist as a CRD listing, only not exposed to admin yet;
  reuse the same query the owner-scoped `/v1` volumes route uses, owner-filtered), the last 5
  `QuotaRequest`s for that owner (`?owner=` from Task 2), and the last 10 `audit::list` rows whose
  `target == slug`).
- Modifies: `PUT /admin/quota/{owner}` body becomes `{ spec: crd::QuotaSpec, note: String }`
  (`note` required non-empty — Global Constraint: "Deny, set quota, roll, drain, decommission,
  add/remove superadmin: required note"); `write_quota_route` extracts `spec` and calls
  `write_quota(&s, &owner, spec)` unchanged, then `audit::record` with `note`.

- [ ] **Step 1: Write the failing tests**

```rust
/// An owner using only the defaults with no request history is listed, unlike today's
/// `usage_all` — the spec's Owners list is "every owner with a Quota object OR any live object",
/// wider than "has a Quota or has ever requested one".
#[tokio::test]
async fn owners_list_includes_an_owner_with_only_live_objects_and_no_quota_or_request() { /*
    create a Workspace for "carol" with no Quota, no QuotaRequest; assert she appears in
    GET /admin/owners with source: "default" */ }

/// The detail composes every section the spec's screen needs, for one owner.
#[tokio::test]
async fn owner_detail_composes_usage_limit_objects_requests_and_audit() { /* one workspace, one
    decided request, one audit row for that owner; assert all four sections are present and
    correctly scoped to that owner only (a second owner's rows must not leak in) */ }

/// A missing note on a quota write is a 422, not a silent no-reason write.
#[tokio::test]
async fn write_quota_without_a_note_is_422() { /* PUT /admin/quota/acme with note: "" → 422 */ }
```

- [ ] **Step 2: Widen owner enumeration**

  `owners_list` (renamed from `usage_all`, kept as the same route `/admin/usage` for backward
  compatibility is NOT needed — the spec replaces Usage with Owners outright, so
  `/admin/usage` is deleted and `/admin/owners` takes its route slot; the web's Usage tab is
  deleted in Task 11). Add a third owner source: every distinct `rustic-git.io/owner` label value
  across `Workspace`/`Environment`/`Volume` — `Api::<Workspace>::all(...).list(&ListParams::default()
  .labels("rustic-git.io/owner"))`'s label KEYS aren't directly listable via `kube`, so instead list
  every object of each of the three kinds and collect `.spec.owner` (not the label — Global
  Constraint: never enumerate off a label) into the same `BTreeSet` `usage_all` already builds from
  Quota/QuotaRequest. This resolves the ponytail note in the existing `usage_all` doc comment;
  update or remove that comment to say what actually happens now.

- [ ] **Step 3: `owner_detail`**

  One handler, sequential awaits composing the five existing/near-existing calls named in
  Interfaces above; a single `Result` short-circuit is fine here (rung 1: no need for
  `tokio::try_join!` — this is an operator page polled every 10s, not a hot path).

- [ ] **Step 4: Required note on `write_quota_route`**

  Change the body type, validate non-empty, thread through to `audit::record`.

- [ ] **Step 5: Run tests, verify, commit**

  ```bash
  cargo test -p rustic-git-workspaces api_admin_owners -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```

  Commit: "Add the owners list and detail, require a note on every quota write"

---

### Task 4: Clusters — activate/deactivate, detail, drain, undrain, decommission

**Files:**
- Create: `crates/workspaces/src/api/admin/clusters.rs`
- Modify: `crates/workspaces/src/api/admin.rs` (mount; `create_region` gains `PUT
  /admin/clusters/{region}/status` for activate/deactivate — reusing `create_region`'s apply-patch
  shape rather than a new verb, since re-registering IS how a region already changes status per
  its own doc comment)
- Modify: `deploy/k3s/api-rbac.yaml` (`nodes: get,list,patch`)
- Modify: `deploy/k3s/README.md` (release note: re-apply `api-rbac.yaml`)
- Test: `crates/workspaces/tests/api_admin_clusters.rs`

**Interfaces:**
- Produces: `GET /admin/clusters` → `Vec<ClusterRow>` (`ClusterRow { region: String, status:
  String, agents_ready: i64, agents_desired: i64, nodes_ready: i64, nodes_total: i64,
  draining: i64, working_copies: i64, settings_status: "present"|"absent"|"parse-error" }` —
  `agents_*`/`nodes_*` from `list_nodes` (existing) plus the `rustic-git-agent` DaemonSet's
  `WorkloadDoc` (existing, `workloads::list_workloads`'s per-region half); `working_copies` counts
  `Workspace`+`Environment` objects in that region whose `podRef`/equivalent is set (a live
  worktree, same predicate `decommission.rs`'s `is_live_worktree` uses); `settings_status` from
  `GET /admin/settings/clusters/{region}` — `present` if a `ClusterSettings` named `default`
  exists and matches its current `observedGeneration`, `absent` if unset (all-default), never
  `parse-error` today since `k8s-openapi` typed decode fails closed as a 5xx, not a partial
  object — kept as a variant for forward compatibility per the spec's exact wording, always
  `present`/`absent` in practice, noted with a `// ponytail:`).
  `GET /admin/clusters/{region}` → `ClusterDetail { region, status, nodes: Vec<NodeRow>, workloads:
  Vec<workloads::WorkloadDoc>, settings: crd::ClusterSettingsSpec }` (`NodeRow` extends today's
  `NodeDoc` with `working_copies: i64` and `replicas_held: i64`, counted the same way
  `decommission.rs`'s `owned`/`copies` counters are, against that node's name).
  `POST /admin/clusters/{region}/nodes/{node}/drain` (body `{ reason: String }`, required) sets
  `crd::DECOMMISSION_LABEL = "true"` on the named `Node`; `POST
  /admin/clusters/{region}/nodes/{node}/undrain` (body `{ reason: String }`) removes the label AND
  the `DECOMMISSION_STATUS` annotation (so a re-drain starts counting from `draining …` again, not
  a stale `drained` stamp); `POST /admin/clusters/{region}/nodes/{node}/decommission` (body
  `{ reason: String }`) refuses (409, "not drained yet") unless the node's
  `rustic-git.io/decommission-status` annotation starts with `"drained "`, then applies the k3s
  built-in `node.kubernetes.io/unschedulable` cordon (`Api::<Node>::cordon`, `kube`'s helper) —
  the console's own decommission action is "stop scheduling new pods here", never a VM delete,
  matching the spec's Decision 2 exactly ("the operator is told the VM may be deleted (the console
  never deletes the VM)").
- Consumes: `crd::DECOMMISSION_LABEL`, `bins/agent/src/decommission.rs::DECOMMISSION_STATUS`
  (re-exported or duplicated as a `pub const` in `crd` if not already public to `workspaces` — `rg
  -n "pub const DECOMMISSION_STATUS" bins/agent/src crates/workspaces/src` first; if it lives only
  in `bins/agent`, move the const to `crates/workspaces/src/crd/mod.rs` next to
  `DECOMMISSION_LABEL` and have `bins/agent` re-export it, so the two tiers can never spell the
  annotation key differently).

- [ ] **Step 1: Write the failing tests**

```rust
/// The list composes agent readiness, node counts, hosted counts and settings status per region —
/// one row answers "is this region healthy" per the spec.
#[tokio::test]
async fn clusters_list_composes_agents_nodes_and_hosted_counts() { /* one region, one node ready,
    one DaemonSet ready=1/desired=1, one live workspace; assert the row's fields */ }

/// Drain sets the label; undrain clears the label AND the status annotation, so a subsequent
/// drain starts fresh rather than showing a stale `drained` stamp from before.
#[tokio::test]
async fn drain_sets_the_label_and_undrain_clears_label_and_status() { /* PATCH via drain, assert
    label true; PATCH via undrain, assert label absent AND decommission-status annotation absent */ }

/// Decommission refuses a node that has not reached `drained` yet.
#[tokio::test]
async fn decommission_refuses_before_drained() { /* node has decommission-status "draining
    running=1 ..."; POST decommission → 409 */ }

/// Decommission on a drained node cordons it and never deletes anything — asserted by there being
/// no delete call in the mock's recorded calls, same style as decommission.rs's own
/// `assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")))`.
#[tokio::test]
async fn decommission_cordons_a_drained_node_and_deletes_nothing() { /* node has
    "drained 2026-...Z"; POST decommission → 200; assert unschedulable: true patched, and no
    DELETE call anywhere in rec.calls() */ }

/// A missing or empty reason on drain/undrain/decommission is a 422 — Global Constraint: reason
/// required on every write except approve.
#[tokio::test]
async fn drain_without_a_reason_is_422() { /* … */ }
```

- [ ] **Step 2: `GET /admin/clusters` and `/admin/clusters/{region}`**

  Compose the existing `list_nodes`, `active_regions`, `workloads::list_workloads`,
  `quota`/settings reads named above. `working_copies`/`replicas_held` counting mirrors
  `decommission.rs::thin_volumes`'s style (iterate the already-listed CRDs, filter by
  `spec.node_name == node` / a `VolumeReplica` whose `spec.node == node`) rather than adding a
  new kube list — the same `Volume`/`VolumeReplica` lists this router already has `kube::Client`
  access to via `client_for_region`.

- [ ] **Step 3: drain / undrain / decommission handlers**

  Three small handlers, each: `caller()` for the actor name, require non-empty `reason`, `Patch::Merge`
  on the `Node` (label for drain/undrain, `spec.unschedulable` for decommission — via the plain
  merge patch `{"spec": {"unschedulable": true}}`, no need for a dedicated `kube` cordon helper if
  one isn't already a dependency; `rg -n "unschedulable" crates -R` first to confirm the field
  path), then `audit::record` with action `"drain"`/`"undrain"`/`"decommission"`, target
  `{region}/{node}`.

- [ ] **Step 4: RBAC**

  `deploy/k3s/api-rbac.yaml`: `resources: ["nodes"]` verb list gains `"patch"` (the resource has no
  `resourceNames` restriction possible for nodes — matches the spec's own note, "name-restricted is
  impossible for nodes — scoped by label selector in the handler"; the handler-level scoping IS
  Step 3's explicit `reason`-gated, single-named-node PATCH, never a bulk verb). Note in
  `deploy/k3s/README.md`'s release-note list: re-apply `api-rbac.yaml` on this release.

- [ ] **Step 5: Run tests, verify, commit**

  ```bash
  cargo test -p rustic-git-workspaces api_admin_clusters -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```

  Commit: "Add cluster detail with node drain, undrain and decommission"

---

### Task 5: Monitoring — scraped signals

**Files:**
- Create: `crates/workspaces/src/api/admin/monitoring.rs`
- Modify: `crates/workspaces/src/api/admin.rs` (mount `/admin/monitoring/signals`; `bins/api`
  gains a `reqwest`-based in-cluster pod fetch, reusing the pattern `admin/settings.rs`'s
  `PeerClient` already establishes for outbound HTTP from this process)
- Test: `crates/workspaces/tests/api_admin_monitoring.rs`

**Interfaces:**
- Produces: `GET /admin/monitoring/signals` → `Vec<SignalRow>` (`SignalRow { alert: &'static str,
  state: "firing"|"ok"|"unknown", why: &'static str }`) evaluating the subset of
  `deploy/alerts.md`'s table a single (or two, 5s apart) scrape can compute:
  - `NoLeader`: `ownership_is_leader` summed across scraped pods — needs the pod list, not
    Prometheus, so this IS computable; `firing` if the sum != 1.
  - `DbFenceDetected`: `db_fence_detected_total` increase since a cached previous sample (stored
    in `ApiState` behind a `Mutex<Option<(Instant, f64)>>`, ponytail: single-process cache, lost on
    restart — acceptable, a restart just means the first scrape after boot reports `unknown` for
    this one row until a second sample exists).
  - `Http5xxRate`: two scrapes 5s apart (this handler literally scrapes, sleeps 5s via
    `tokio::time::sleep`, scrapes again — the spec's own wording, "5xx ratio via two scrapes 5 s
    apart or a cached previous sample"; the cached-sample path reuses the same `Mutex` slot as
    `DbFenceDetected`'s, keyed by metric name, so a second Signals call within the window is fast).
  - `ReconcileErrors`: same two-scrape or cached-sample ratio over `reconciles_total{result=...}`.
  - `TunnelSaturation`: `gateway_open_tunnels` max across scraped pods, `firing` if any pod > 800.
  - `LeaseRenewFailing`, `MisdirectedWrites`, `WorkerHeartbeatStale`, `PoolAlmostFull`,
    `NodeDiskAlmostFull`: `state: "unknown"` — each needs a rate over a real window (5–10m) or
    node-exporter, neither obtainable from an ad-hoc scrape; `why` says exactly that ("needs a
    sustained rate window a point-in-time scrape cannot compute" / "needs node-exporter, not
    deployed").
  Pods are listed by label — `rg -n "app.kubernetes.io/name\|prometheus.io/scrape" deploy/rustic-git.yaml
  deploy/k3s/*.yaml` for the exact label selector already on each Deployment/StatefulSet's pod
  template — fetched over the in-cluster pod IP at `:9464/metrics` (`RUSTIC_GIT_METRICS_ADDR`'s
  default port per `crates/core/src/metrics.rs`), parsed as Prometheus text exposition (a small
  hand-rolled line parser: `name{labels} value` — ponytail: no `prometheus-parse` crate added for
  ~15 lines of splitting, revisit if the format needs escaping this parser doesn't handle).
  `restarts_last_hour`: from each central pod's `status.containerStatuses[].restartCount` compared
  against nothing stored (Kubernetes does not expose "restarts in the last hour" directly) —
  ponytail: reports `restartCount` since pod start, not a true 1h window; a true window needs
  either an events lookup (`kubectl get events --field-selector involvedObject.name=...`,
  noisy and short-retention) or a stored baseline this plan does not add. Named `restarts` in the
  response, not `restarts_last_hour`, to avoid asserting a precision the field doesn't have; the
  Monitoring page's copy says "since the pod started" rather than "in the last hour" to match
  (spec ambiguity noted below).

- [ ] **Step 1: Write the failing tests**

```rust
/// The text-exposition parser reads a value out of a Prometheus scrape body, ignoring comment
/// and TYPE/HELP lines and labels on other series.
#[test]
fn parses_a_named_gauge_out_of_prometheus_text() { /* "# HELP x\n# TYPE x gauge\nownership_is_leader
    1\nsome_other_metric 5\n" → sum_of("ownership_is_leader", text) == 1.0 */ }

/// NoLeader fires when the summed value across pods is not exactly 1.
#[test]
fn no_leader_fires_on_zero_or_two() {
    assert_eq!(evaluate_no_leader(0.0), "firing");
    assert_eq!(evaluate_no_leader(1.0), "ok");
    assert_eq!(evaluate_no_leader(2.0), "firing");
}

/// A rule needing a sustained window is unknown, never guessed as ok.
#[test]
fn window_only_rules_report_unknown() {
    let rows = signal_rows(&ScrapeSample::empty());
    let stale = rows.iter().find(|r| r.alert == "WorkerHeartbeatStale").unwrap();
    assert_eq!(stale.state, "unknown");
}
```

- [ ] **Step 2: the text-exposition parser and per-rule evaluators**

  Pure functions (`sum_of(name, text) -> f64`, `evaluate_no_leader`, `evaluate_fence`,
  `evaluate_ratio(before, after) -> f64`, `evaluate_tunnels`), each independently unit-tested
  before the handler wires them to a real scrape — this is the module's actual logic, the HTTP
  fetch is thin plumbing around it.

- [ ] **Step 3: the handler**

  List central pods (`Api::<Pod>::namespaced(aks_client, "rustic-git")` filtered by the label
  selector found in Step's file check above), `reqwest::get` each `/metrics`, evaluate, assemble
  `Vec<SignalRow>` in `deploy/alerts.md`'s table order. `restarts_last_hour` reads
  `status.container_statuses` off the same pod list, no second fetch.

- [ ] **Step 4: Run tests, verify, commit**

  ```bash
  cargo test -p rustic-git-workspaces api_admin_monitoring -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```

  Commit: "Scrape and evaluate the alert catalogue on the Monitoring page"

---

### Task 6: Access — cannot-remove-yourself, cannot-remove-the-last-one, add requires an existing user

**Files:**
- Modify: `crates/api/src/teams.rs` (`add_superadmin`, `remove_superadmin`)
- Test: `crates/api/src/teams.rs` (inline `#[cfg(test)]`, or wherever `directory` is stubbed for
  this module — `rg -n "trait Directory\|struct StubDirectory" crates/api/src` first)

**Interfaces:**
- Modifies: `remove_superadmin` refuses (409) when `user == caller` ("remove yourself" — the
  spec's exact rule) and when `db.superadmins().await?.len() == 1` and that one row's user is the
  target ("cannot remove the last one").
- Modifies: `add_superadmin` refuses (422) when `user` names nobody in the directory — needs a
  `Directory::user_exists(&self, email: &str) -> Result<bool, Error>` or equivalent lookup; `rg -n
  "trait Directory" crates/pulls/src/directory/mod.rs` for whichever existing method already
  answers "does this email have an account" (`find_by_email` or similar) rather than adding a new
  one if one already exists.

- [ ] **Step 1: Write the failing tests**

```rust
/// A superadmin cannot remove their own claim through this route — the spec's exact wording.
#[tokio::test]
async fn remove_superadmin_refuses_to_remove_yourself() { /* caller == "a@x", target == "a@x",
    two superadmins exist → 409 */ }

/// The last superadmin cannot be removed by anyone, even another superadmin.
#[tokio::test]
async fn remove_superadmin_refuses_to_remove_the_last_one() { /* one superadmin "a@x", caller is
    a DIFFERENT superadmin "b@x" (contrived: seed two, remove one first, then try removing the
    last) → 409 on the second removal */ }

/// Adding an email with no account is refused rather than minting a claim for nobody.
#[tokio::test]
async fn add_superadmin_refuses_an_email_with_no_account() { /* "ghost@x" has no directory row →
    422 */ }
```

- [ ] **Step 2: Implement the three rules**

  In order, each a cheap early return before the existing `db.add_superadmin`/`remove_superadmin`
  call: self-removal check (string compare against `caller`), last-one check
  (`db.superadmins().await` length), existing-user check (the directory lookup). Each returns its
  own status/message per the constraint table above (409 for the two removal rules — they are
  refusing a legitimate but currently-unsafe action, matching the spec's "409 shows the conflicting
  state"; 422 for add — the request names a subject that does not exist, matching "422 shows the
  field and range" generalized to "the field and why").

- [ ] **Step 3: Run tests, verify, commit**

  ```bash
  cargo test -p rustic-git-api teams -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```

  Commit: "Refuse removing yourself, removing the last superadmin, and adding a nonexistent user"

---

### Task 7: Overview

**Files:**
- Create: `crates/workspaces/src/api/admin/overview.rs`
- Modify: `crates/workspaces/src/api/admin.rs` (mount `/admin/overview`)
- Test: `crates/workspaces/tests/api_admin_overview.rs`

**Interfaces:**
- Produces: `GET /admin/overview` → `Overview { pending_requests: Vec<QuotaRequestDoc>, attention:
  Vec<AttentionItem>, recent_audit: Vec<audit::AuditEntry>, fleet: FleetNumbers }`.
  `AttentionItem { kind: &'static str, detail: String, href: String }` for: workloads not fully
  ready (`ready < desired` in `workloads::list_workloads`'s output), nodes NotReady or draining
  (`list_nodes` output), a region with zero ready agents (`ClusterRow.agents_ready == 0`), a
  cluster whose settings failed to parse (`ClusterRow.settings_status == "parse-error"`), and
  every `SignalRow` with `state == "firing"` from Task 5. `FleetNumbers { owners: i64, workspaces:
  i64, environments: i64, snapshots: i64, disk_gb_total: i64, per_region:
  BTreeMap<String, RegionFleet> }` computed the same way `owners_all`/`list_workloads` already
  enumerate — no new counters, no cache (Global Constraint).
- Consumes: Tasks 1, 3, 4, 5's handlers directly (in-process function calls, not HTTP —
  `owners::owner_rows(&s).await?`, `clusters::cluster_rows(&s).await?`,
  `monitoring::signal_rows(&s).await?`, `list_all_quota_requests`'s inner logic factored so
  `overview.rs` can call it without an HTTP round trip; extract each route's body into a plain
  `async fn` the route handler then wraps in `Json(...).into_response()`, same shape
  `workload_doc`/`list_workloads` already have in `workloads.rs`).

- [ ] **Step 1: Write the failing test**

```rust
/// One call composes every card the landing page needs — the spec's "one round trip for the
/// landing page".
#[tokio::test]
async fn overview_composes_pending_attention_audit_and_fleet() { /* one pending request, one
    NotReady node, one audit row, two workspaces across two regions; assert all four sections
    populated and fleet numbers match a hand count */ }

/// Nothing pending and nothing firing is the documented empty state's data shape — an empty
/// `pending_requests` and `attention`, fleet numbers still populated.
#[tokio::test]
async fn overview_with_nothing_pending_still_returns_fleet_numbers() { /* … */ }
```

- [ ] **Step 2: Factor the three composed handlers into plain functions**

  `list_all_quota_requests` (Task 2), `owners::owner_rows`/`clusters::cluster_rows` (Tasks 3–4),
  `monitoring::signal_rows` (Task 5) each already return a `Result<Response, Response>` shaped
  route body; split each into an inner `async fn ..._inner(&s) -> Result<Vec<T>, Response>` the
  route wraps with `Json(...).into_response()` and `overview.rs` calls directly. This is a
  refactor of Steps already written in Tasks 2–5 — do it as part of THIS task's diff (touching
  those files again) rather than pre-emptively in each earlier task, since the shape only becomes
  obvious once Overview needs to call all four.

- [ ] **Step 3: `overview_handler`**

  Await the four inner calls, compute `FleetNumbers` from the same owner/workspace/environment
  listings `owners::owner_rows` already fetched (pass the already-fetched data through rather than
  re-listing).

- [ ] **Step 4: Run tests, verify, commit**

  ```bash
  cargo test -p rustic-git-workspaces api_admin_overview -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```

  Commit: "Add the superadmin Overview endpoint"

---

### Task 8: Web shell — rail, the eight areas, `place()`

**Files:**
- Modify: `web/apps/web/src/components/app/shell-nav.tsx` (`SUPERADMIN_TABS` replaced; `place()`
  unchanged — `superadmin` kind already exists and needs no new discrimination since every area is
  still under `/superadmin/*`)
- Create: `web/apps/web/src/app/(shell)/superadmin/rail.tsx`
- Modify: `web/apps/web/src/app/(shell)/superadmin/layout.tsx` (render the rail; on desktop
  alongside `children`, collapsing to the existing tab row on narrow — CSS breakpoint, matching
  whatever pattern the product's own left-rail-vs-tabs (if any exists — `rg -n "lg:flex\|md:hidden"
  web/apps/web/src/components/app/app-shell.tsx` for the breakpoint convention) already uses)
- Delete: `web/apps/web/src/app/(shell)/superadmin/[...rest]/page.tsx`'s 404 catch-all is KEPT
  (spec: "catch-all 404 kept") — no change to that file
- Test: `web/apps/web/src/lib/superadmin-nav.test.ts` (new, pure)

**Interfaces:**
- Produces: `web/apps/web/src/lib/superadmin-nav.ts` — `SUPERADMIN_AREAS: { href: string; label:
  string }[]`, the eight rows from the spec's Information architecture table (`/superadmin`
  "Overview", `/superadmin/requests` "Requests", `/superadmin/owners` "Owners",
  `/superadmin/clusters` "Clusters", `/superadmin/monitoring` "Monitoring", `/superadmin/audit`
  "Audit", `/superadmin/access` "Access", `/superadmin/configuration` "Configuration") and
  `activeArea(pathname: string): string` (longest-prefix match against `SUPERADMIN_AREAS`, since
  `/superadmin/owners/acme` must highlight "Owners" — pure function, easy to test without a DOM).

- [ ] **Step 1: Write the failing test**

```ts
import { describe, expect, test } from "bun:test";
import { SUPERADMIN_AREAS, activeArea } from "./superadmin-nav";

describe("activeArea", () => {
  test("matches the exact area", () => {
    expect(activeArea("/superadmin/requests")).toBe("/superadmin/requests");
  });
  test("matches a detail page under an area by longest prefix", () => {
    expect(activeArea("/superadmin/owners/acme")).toBe("/superadmin/owners");
  });
  test("matches Overview only at the root, never as a prefix of every other area", () => {
    expect(activeArea("/superadmin/audit")).toBe("/superadmin/audit");
    expect(activeArea("/superadmin")).toBe("/superadmin");
  });
});
```

- [ ] **Step 2: `lib/superadmin-nav.ts`**

  `activeArea` sorts `SUPERADMIN_AREAS` by href length descending, returns the first whose href
  equals the pathname or is a prefix of it followed by `/` — except `/superadmin` itself, which
  only matches the exact root (otherwise it would prefix-match everything).

- [ ] **Step 3: `rail.tsx`**

  Client component (`"use client"`, needs `usePathname`), renders `SUPERADMIN_AREAS` as a vertical
  list on desktop (`lg:flex lg:flex-col lg:w-48`) and delegates to the existing `NavTabs` row on
  narrow screens (`lg:hidden`) — same items, `activeArea(usePathname())` decides the highlighted
  one either way. `shell-nav.tsx`'s `SUPERADMIN_TABS` constant is deleted; `ShellTabs`'s
  `at.kind === "superadmin"` branch now renders nothing (or a thin breadcrumb-only strip), since
  the rail lives inside the `/superadmin` layout, not the top shell tab row — the product's org tab
  row was for owners, and superadmin's own navigation now belongs to its own layout, matching the
  spec's "the rail because there are seven [eight] areas and the product's top tab row is for the
  org level".

- [ ] **Step 4: `layout.tsx`**

  Two-column flex on desktop (`rail.tsx` + `children`), stacked on narrow (tab row above content) —
  `requireSuperadmin` call unchanged.

- [ ] **Step 5: Verify, commit**

  ```bash
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  bun test; echo exit=$?
  ```

  Commit: "Replace the superadmin tab row with the eight-area rail"

---

### Task 9: Web Overview

**Files:**
- Modify: `web/apps/web/src/app/(shell)/superadmin/page.tsx` (becomes Overview; today's Requests
  content moves to Task 10's `requests/page.tsx`)
- Modify: `web/apps/web/src/lib/api.ts` (`adminOverview(token)`)

**Interfaces:**
- Produces: `adminOverview(token: string): Promise<ApiResult<Overview>>` calling `adminCall<Overview>("/admin/overview", { method: "GET", token })`.

- [ ] **Step 1: `api.ts`**

  One typed call, same shape as every other `adminCall` wrapper in the file (`rg -n "export
  function admin" web/apps/web/src/lib/api.ts` for the exact pattern to copy).

- [ ] **Step 2: `page.tsx`**

  Four cards: Pending (top 3 by age + count + link to `/superadmin/requests`), Attention (each
  `AttentionItem` as a row with its `href`), Recent activity (last 10 `recent_audit` rows, link to
  `/superadmin/audit`), Fleet numbers (owners/workspaces/environments/snapshots/disk, per region —
  a small table). Empty state: when `pending_requests.length === 0 && attention.length === 0`,
  one sentence ("Nothing needs attention.") plus the fleet numbers, per spec.

- [ ] **Step 3: Verify, commit**

  ```bash
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  ```

  Commit: "Add the superadmin Overview page"

---

### Task 10: Web Requests — decision panel with editable approve

**Files:**
- Create: `web/apps/web/src/app/(shell)/superadmin/requests/page.tsx` (moved/adapted from today's
  `superadmin/page.tsx`)
- Create: `web/apps/web/src/app/(shell)/superadmin/requests/decision-panel.tsx`
- Modify: `web/apps/web/src/app/(shell)/superadmin/actions.ts` (`decideRequest` gains an edited
  `requested` argument)
- Modify: `web/apps/web/src/lib/api.ts` (`adminDecideQuotaRequest` gains an optional `requested`
  param; `adminListQuotaRequests` gains `?owner=&state=`)

**Interfaces:**
- Modifies: `adminDecideQuotaRequest(id, decision, note, token, requested?:
  Partial<Record<QuotaDim, number>>)` — POSTs `{ note, requested }` (both optional per Task 2's
  body shape).
- Modifies: `adminListQuotaRequests(token, filter?: { owner?: string; state?: string })`.

- [ ] **Step 1: `decision-panel.tsx`**

  Client component: shows the same facts the row already shows (current → requested per changed
  dimension, reason, current usage/limit) plus the owner's last three decided requests (from
  `adminListQuotaRequests(token, { owner, state: "approved" })` /`"denied"` merged, sliced to 3 —
  or, cheaper, request-level: the caller passes the already-fetched full list down and the panel
  filters client-side, since the whole queue is already on the page). An editable copy: one
  number input per dimension the request touched, defaulting to the requested value; Approve
  submits `{ note, requested: <edited values> }`; Deny submits `{ note }` with the note required
  (client-side non-empty check mirroring the server's 422, so the operator sees the problem before
  the round trip).

- [ ] **Step 2: `page.tsx`**

  Filters (person/team via a text input matching owner substring — client-side, no new API param
  needed for a slug substring; dimension and age filters likewise client-side over the already-small
  fetched list, per rung 1 — a server-side filter is Task 2's `?owner=&state=`, used for the
  Pending/Decided split, not for the free-text/dimension/age narrowing which has no obvious
  server-side shape worth adding for a queue this size). Row click opens `decision-panel.tsx`
  (a `<details>`/disclosure or a client-side toggle state — no modal library, per the ladder).
  The 409 (decided by someone else meanwhile) surfaces via `conflictMessage` (already imported in
  `actions.ts`) as an inline error on the panel, not a toast.

- [ ] **Step 3: Verify, commit**

  ```bash
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  ```

  Commit: "Add the Requests decision panel with editable approve"

---

### Task 11: Web Owners — list, Defaults card, detail

**Files:**
- Create: `web/apps/web/src/app/(shell)/superadmin/owners/page.tsx`
- Create: `web/apps/web/src/app/(shell)/superadmin/owners/[slug]/page.tsx`
- Delete: `web/apps/web/src/app/(shell)/superadmin/usage/page.tsx`,
  `web/apps/web/src/app/(shell)/superadmin/quotas/page.tsx` (folded in, per spec: "Quota defaults
  live under Owners... not as their own area")
- Modify: `web/apps/web/src/lib/api.ts` (`adminOwners(token)`, `adminOwnerDetail(slug, token)`,
  `adminWriteQuota` gains the required `note` param)
- Modify: `web/apps/web/src/app/(shell)/superadmin/actions.ts` (`setQuota` action requires a note;
  the existing `writeDefault` action, reused for the Defaults card, also gains one)

**Interfaces:**
- Produces: `adminOwners`, `adminOwnerDetail` typed calls per Task 3's response shapes.
- Modifies: `adminWriteQuota(owner, spec, note, token)`.

- [ ] **Step 1: `owners/page.tsx`**

  Defaults card at top (today's `quotas/page.tsx` content, reusing `writeDefault` — add the note
  field to its form, matching Task 3's server-side requirement, plus the "current fleet max" hint
  per dimension the spec calls for: computed client-side as `Math.max(...rows.map(r => r.used[dim]))`
  over the already-fetched owner rows, no new endpoint). List below: sortable by tightest-dimension
  usage ratio (`Math.max(...DIMS.map(d => used[d] / limit[d]))`, client-side sort — the whole owner
  list is small enough this needs no server-side sort param), search by slug (client-side filter),
  pending-request badge from `row.pending`.

- [ ] **Step 2: `owners/[slug]/page.tsx`**

  Six dimension bars (reuse `quota-bar.tsx` from the quotas-and-superadmin plan, `rg -n
  "quota-bar" web/apps/web/src/components/app` to confirm it still exists under that name), Set
  quota form (note required, pre-filled with the owner's own limit or the default if `source ===
  "default"`), workspaces/environments tables (state, node, region, age — reuse whatever row
  component the owner's own `/[owner]/workspaces` page already renders, `rg -n "WorkspaceRow\|function
  Row" web/apps/web/src/app/\(shell\)/\[owner\]/workspaces` first), volumes (detached ones flagged),
  request history (last 5 from the detail payload, link to `/superadmin/requests?owner=slug` for
  the rest), audit trail (last 10, link to `/superadmin/audit?target=slug`).

- [ ] **Step 3: Verify, commit**

  ```bash
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  ```

  Commit: "Add the Owners list, Defaults card and detail page"

---

### Task 12: Web Clusters — list + detail with drain/undrain/decommission

**Files:**
- Modify: `web/apps/web/src/app/(shell)/superadmin/clusters/page.tsx` (region cards, replacing
  today's content — `rg -n . web/apps/web/src/app/\(shell\)/superadmin/clusters/page.tsx` first to
  see what it currently renders and keep the Add region form and its `createRegionAction`)
- Create: `web/apps/web/src/app/(shell)/superadmin/clusters/[region]/page.tsx`
- Modify: `web/apps/web/src/app/(shell)/superadmin/actions.ts` (`drainNode`, `undrainNode`,
  `decommissionNode` actions, each requiring a reason; `activateRegion`/`deactivateRegion`)
- Modify: `web/apps/web/src/lib/api.ts` (`adminClusters`, `adminClusterDetail`, `adminDrainNode`,
  `adminUndrainNode`, `adminDecommissionNode`, `adminSetRegionStatus`)

**Interfaces:**
- Produces: typed calls for every Task 4 route.

- [ ] **Step 1: `clusters/page.tsx`**

  One card per region: name, status badge, agents ready/desired, nodes ready/total, draining
  count (nodes whose `decommission_status` starts with `"draining"`), working copies, settings
  status. Activate/deactivate toggle (`activateRegion`/`deactivateRegion` actions, second
  confirmation for deactivate per Global Constraint). Add region form kept from today's page.

- [ ] **Step 2: `clusters/[region]/page.tsx`**

  Nodes table (name, ready, agent pod ready — cross-referenced from the region's `workloads` list
  by `name === "rustic-git-agent"`'s readiness doesn't map per-node, so agent-pod-readiness per
  node needs the DaemonSet's `status.numberReady` shown once per region, not per node — spec
  ambiguity noted below, resolved as: show the DaemonSet-level ready/desired once above the table,
  and per-node only what `NodeRow` actually carries — `ready`, `decommission_status` parsed into
  the four counters via a small client-side regex/split (`draining running=(\d+) owned=(\d+)
  copies=(\d+) thin=(\d+)` or `drained <timestamp>`), `working_copies`, `replicas_held`). Row
  actions: Drain (confirmation dialog naming the consequence — "running work keeps running but
  nothing new lands here" — reason required), Undrain, Decommission (only enabled once
  `decommission_status` starts with `"drained "`; SECOND confirmation naming "the node is cordoned;
  its VM may now be deleted", per Global Constraint and spec Decision 2). Workloads section:
  agent DaemonSet and gateway Deployment for this region, Roll with reason (reuse
  `rollWorkloadAction`). Settings summary: read-only render of `GET /admin/settings/clusters/{region}`
  (reuse whatever component the existing cluster settings tab already has, if the quotas-and-
  superadmin plan built one — `rg -rn "settings/clusters" web/apps/web/src` — link to
  `/superadmin/configuration` per spec, no edit control here).

- [ ] **Step 3: Live counters while a drain is in progress**

  Per Global Constraint, poll at 2s instead of 10s while this page has an active drain/undrain/
  decommission in flight (client-side `useEffect` interval swap keyed on whether any node's status
  starts with `"draining"`) — a small client component wrapping the server-rendered table with a
  `router.refresh()` timer, matching whatever the product's existing "faster while X in progress"
  polling pattern is (`rg -rn "router.refresh\(\)" web/apps/web/src/app` for the pattern to copy;
  if the pattern doesn't exist yet in a reusable form, a 15-line client component is the lazy
  right size — no polling library).

- [ ] **Step 4: Verify, commit**

  ```bash
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  ```

  Commit: "Add the Clusters list and detail with drain, undrain and decommission"

---

### Task 13: Web Monitoring — Signals table

**Files:**
- Modify: `web/apps/web/src/app/(shell)/superadmin/monitoring/page.tsx` (add the Signals section
  below today's workloads table — `rg -n . web/apps/web/src/app/\(shell\)/superadmin/monitoring/page.tsx`
  first)
- Modify: `web/apps/web/src/lib/api.ts` (`adminMonitoringSignals(token)`)

**Interfaces:**
- Produces: `adminMonitoringSignals(token): Promise<ApiResult<SignalRow[]>>`.

- [ ] **Step 1: `api.ts`**

  One typed call.

- [ ] **Step 2: `page.tsx`**

  Table: alert name, state badge (firing = destructive variant, ok = outline, unknown = muted),
  the `why` text. `RUSTIC_GIT_GRAFANA_URL` link shown only if set — reuse whatever the app already
  does to read an env-derived optional link (`rg -n "NEXT_PUBLIC\|process.env" web/apps/web/src/app/\(shell\)/superadmin`
  for the pattern; if `RUSTIC_GIT_GRAFANA_URL` isn't already surfaced to the web, add it via
  `getPublicCentralSettings` if that's where such links live, else a plain `NEXT_PUBLIC_GRAFANA_URL`
  env var read server-side in the page).

- [ ] **Step 3: Verify, commit**

  ```bash
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  ```

  Commit: "Add the scraped alert signals to Monitoring"

---

### Task 14: Web Audit — filters and CSV

**Files:**
- Create: `web/apps/web/src/app/(shell)/superadmin/audit/page.tsx`
- Create: `web/apps/web/src/lib/audit.ts` (+ `.test.ts`)
- Modify: `web/apps/web/src/lib/api.ts` (`adminAudit(token, filter)`)

**Interfaces:**
- Produces: `web/apps/web/src/lib/audit.ts`: `auditQueryString(filter: AuditFilter): string` (pure,
  URL-encodes only the set fields, stable key order so it's testable byte-for-byte).
  `adminAudit(token, filter): Promise<ApiResult<AuditPage>>` GETs `/admin/audit${auditQueryString(filter)}`.

- [ ] **Step 1: Write the failing test**

```ts
import { describe, expect, test } from "bun:test";
import { auditQueryString } from "./audit";

describe("auditQueryString", () => {
  test("encodes only the fields that are set, in a stable order", () => {
    expect(auditQueryString({ actor: "op@x.com", action: "" })).toBe("?actor=op%40x.com");
  });
  test("empty filter is an empty string", () => {
    expect(auditQueryString({})).toBe("");
  });
});
```

- [ ] **Step 2: `lib/audit.ts`**

  Field order `actor, action, target, from, to, cursor, limit`; skip empty/undefined.

- [ ] **Step 3: `audit/page.tsx`**

  Filter form (actor, action select from the fixed action-word list Task 1 writes, target,
  date-from/to), table (`ts, actor, action, target, reason, result` with `tabular-nums` per Global
  Constraint), pagination via `next_cursor` (a "Load more" button, not infinite scroll, per rung 1 —
  the simplest thing that works for an operator paging through a bounded queue). CSV export: a
  plain `<a href="{ADMIN_BASE}/admin/audit.csv?...">` is wrong (needs the bearer token) — instead a
  server action that fetches the CSV server-side and returns it via a `Response` with
  `Content-Disposition: attachment`, OR (simpler, per the ladder) a client button that calls a new
  tiny route handler `app/(shell)/superadmin/audit/export/route.ts` which forwards the token-bearing
  fetch and streams the CSV back — pick whichever the app already has a precedent for (`rg -rn
  "route.ts" web/apps/web/src/app` for any existing download/export route to copy the shape of).

- [ ] **Step 4: Verify, commit**

  ```bash
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  bun test; echo exit=$?
  ```

  Commit: "Add the Audit page with filters and CSV export"

---

### Task 15: Web Access

**Files:**
- Create: `web/apps/web/src/app/(shell)/superadmin/access/page.tsx`
- Modify: `web/apps/web/src/app/(shell)/superadmin/actions.ts` (`addSuperadminAction`,
  `removeSuperadminAction`)

**Interfaces:**
- Consumes: existing `listSuperadmins`, `addSuperadmin`, `removeSuperadmin` from `api.ts` (already
  present per the earlier grep — lines 1137/1141/1145).

- [ ] **Step 1: `access/page.tsx`**

  Table: email, added by, added at (from `listSuperadmins`'s existing row shape — `rg -n
  "listSuperadmins\|SuperAdminRow" web/apps/web/src/lib/api.ts` for the exact fields). Add form
  (email input, calls `addSuperadminAction`, surfaces the 422 "no account" error inline). Remove
  button per row, disabled (with a tooltip, not hidden — an operator should see WHY, per "dangerous
  is loud") on the caller's own row and on the row when only one remains (client-side mirror of
  Task 6's two rules, computed from the already-fetched list plus the current session's own email
  via `requireSuperadmin`'s session). Second confirmation on remove, per Global Constraint. Bootstrap
  email shown with a small badge ("bootstrap") — the row whose `addedBy` is empty/a sentinel the
  bootstrap path writes (`rg -n "bootstrap" crates/pulls/src/directory/mod.rs` for the exact
  sentinel value to match against).

- [ ] **Step 2: Verify, commit**

  ```bash
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  ```

  Commit: "Add the Access page for managing superadmins"

---

### Task 16: Web Configuration (read-only)

**Files:**
- Create: `web/apps/web/src/app/(shell)/superadmin/configuration/page.tsx`

**Interfaces:**
- Consumes: existing `GET /admin/settings/schema` (`crates/workspaces/src/api/admin/schema.rs`,
  already built) — no new backend route. `rg -n "getSchema\|settingsSchema" web/apps/web/src/lib/api.ts`
  for whether a typed call already exists; add one (`adminSettingsSchema(token)`) if not.

- [ ] **Step 1: `configuration/page.tsx`**

  Two tables (central, cluster) rendering the schema rows: name, description, unit, range,
  default, env override (if set), mark (live/boot), readers. No form controls anywhere on this
  page — read-only per spec ("No editing; a sentence says where each is changed"). One sentence at
  the top: "Central settings are changed under Monitoring's server row" is wrong — actually they're
  changed via `PUT /admin/settings/central`, which has no dedicated page in this plan (it was the
  quotas-and-superadmin plan's own settings tabs, superseded per this spec's header note "Supersedes
  the admin-area paragraphs... as far as the web is concerned; every backend route those specs
  added stays and is reused here" — meaning the SETTINGS EDITOR ui itself is out of scope for this
  console per "Not doing: Editing tunables in the UI"). The sentence instead says: "Stored values
  are changed through the admin API directly (`PUT /admin/settings/central` /
  `/admin/settings/clusters/{region}`); this page is read-only," matching "a sentence says where
  each is changed (deploy manifest, or the admin API for the stored ones)" verbatim.

- [ ] **Step 2: Verify, commit**

  ```bash
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  ```

  Commit: "Add the read-only Configuration page"

---

### Task 17: Docs and e2e

**Files:**
- Modify: `CLAUDE.md` (superadmin console paragraph, near the existing admin-process description)
- Modify: `deploy/k3s/README.md` (release note: re-apply `api-rbac.yaml` for `nodes: patch`)
- Modify: `tests/ws_e2e.sh`

**Interfaces:** none (docs + shell script).

- [ ] **Step 1: `CLAUDE.md`**

  One paragraph after the existing admin-process description (`crates/workspaces/src/api/admin.rs`
  paragraph, if the file already has one from the quotas-and-superadmin plan — `rg -n "admin.rs\|
  superadmin" CLAUDE.md`), naming: the audit log's key shape and "kept forever, no pruning route
  in this plan"; drain vs decommission as two distinct actions (drain = label, work keeps running;
  decommission = cordon after drained, VM deletion is a human's separate step); the eight web
  areas and where each backend route lives.

- [ ] **Step 2: `deploy/k3s/README.md`**

  Release note entry: "Superadmin console: re-apply `deploy/k3s/api-rbac.yaml` (`nodes` gains
  `patch`) before deploying this release's `rustic-git-admin` image — the drain/undrain/
  decommission routes 403 without it."

- [ ] **Step 3: `tests/ws_e2e.sh`**

  Add, guarded the same way the file's existing admin-route checks are (`rg -n "ADMIN_BASE"
  tests/ws_e2e.sh` for the existing pattern): drain a node via `$ADMIN_BASE/admin/clusters/{region}/nodes/{node}/drain`,
  assert the label lands (`kubectl get node {node} -o jsonpath=...`), undrain, assert it clears;
  then one audit-row check: `curl $ADMIN_BASE/admin/audit?action=drain` returns at least one row
  for that node. Exit 77 semantics unchanged (a missing prerequisite skips, doesn't fail) — this
  addition runs only inside the existing k3s-cluster guarded section, not as a new top-level
  prerequisite.

- [ ] **Step 4: Run the gates one more time across the whole plan**

  ```bash
  cargo test -p rustic-git-workspaces -p rustic-git-api -p rustic-git-agent-bin -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  cd web && bun run lint; echo exit=$?
  bunx tsc --noEmit -p apps/web/tsconfig.json; echo exit=$?
  bun test; echo exit=$?
  ```

  Commit: "Document the superadmin console and cover node drain in the workspaces e2e suite"

---

## Self-review

### Spec coverage

| Spec section | Covered by |
|---|---|
| Overview | Task 7 (backend), Task 9 (web) |
| Requests (queue, decision panel, approve-with-edits, Decided tab, 409) | Task 2, Task 10 |
| Owners (list, Defaults card, detail, Set quota with note) | Task 3, Task 11 |
| Clusters (list, detail, drain, undrain, decommission, add/activate/deactivate region, roll) | Task 4, Task 12 |
| Monitoring (workloads table — already existed; Signals) | Task 5, Task 13 |
| Audit (log writer on every write, filters, CSV) | Task 1, Task 14 |
| Access (list/add/remove already existed; the three rules) | Task 6, Task 15 |
| Configuration (schema endpoint already existed; read-only page) | Task 16 |
| Cross-cutting: permissions | unchanged — `refuse_without_claim` already gates the whole router |
| Cross-cutting: reason on every write except approve | Tasks 1, 2, 3, 4, 6 each enforce it on their own routes |
| Cross-cutting: second confirmation (server roll, node drain, remove superadmin, deactivate region) | Task 12 (drain/decommission), Task 15 (remove), Task 12 (deactivate); server StatefulSet roll's second confirmation is UI-only (Task 13, unchanged backend) |
| Cross-cutting: freshness (10s/2s poll) | Task 12 Step 3 is the one page with an in-progress state; other pages use the app's existing auto-refresh, unchanged |
| Cross-cutting: errors (409/422/5xx shapes) | inherited from existing `kube_err`/range_err/conflict helpers; Task 2's approve 409 and Task 4's decommission 409 are new instances of the existing shape |
| Cross-cutting: density/layout, vocabulary | Task 8 (rail), vocabulary enforced throughout by Global Constraints |
| Decisions: approve-with-edits | Task 2 |
| Decisions: drain AND decommission from the console | Task 4, Task 12 |
| Decisions: audit kept forever | Task 1 — no pruning route, no TTL, stated explicitly in Global Constraints |
| Backend gaps 1–7 (spec's own list) | 1→Task 2, 2→Task 3, 3→Task 4, 4→Task 5, 5→Task 1, 6→Task 6, 7→Task 7 |
| Phasing: Phase 1 scope only | followed; Phase 2 (Prometheus/Alertmanager, notifications, scheduled reports) untouched |
| Not doing | followed — no settings editor UI (Task 16 is read-only), no Kubernetes object browser, no impersonation, no per-user notifications |

### Placeholder scan

No task step says "similar to Task N" as a substitute for real code — every step names the exact
file, the exact function/route, and either inline Rust/TS or a precise description of the
composition (which existing functions it calls and in what order). Where a step defers a detail
to "read the file first" (e.g., Task 4's `unschedulable` field path, Task 15's bootstrap sentinel),
that is a genuine unknown this plan cannot resolve without running code in this repo — each such
spot names exactly what to `rg` for and what decision the answer feeds, not a placeholder for
logic.

### Type consistency

`AuditEntry`, `OwnerRow`/`OwnerDetail`, `ClusterRow`/`ClusterDetail`/`NodeRow`, `SignalRow`,
`Overview`/`AttentionItem`/`FleetNumbers` are each defined once (Tasks 1/3/4/5/7) and reused by
every task that composes them (Task 7 composes 2/3/4/5's inner functions rather than redefining
their shapes; the web's `api.ts` typed calls in Tasks 9–16 each name the matching Rust struct's
field set with `camelCase` serde renaming assumed consistent with the rest of `api.ts`'s existing
typed calls, none of which is re-typed independently per page).

### Spec ambiguities resolved

1. **"restarts in the last hour"** (Monitoring): Kubernetes exposes cumulative
   `restartCount` since pod start, not a rolling 1h window. Resolved in Task 5 as reporting
   `restartCount` with UI copy saying "since the pod started," not "in the last hour" — a
   ponytail-marked simplification rather than building an events-based rolling window this plan
   has no other reason to need.
2. **Node table's "agent pod ready" column** (Clusters detail): the agent runs as a DaemonSet, one
   pod per node, but the readiness signal this plan already has (`workloads::WorkloadDoc`) is
   DaemonSet-level (`numberReady`/`desiredNumberScheduled`), not per-node. Resolved in Task 12 as
   showing the DaemonSet's readiness once above the node table rather than inventing a per-node
   pod lookup with no existing route behind it; the per-node `NodeRow` carries only what `list_nodes`
   already exposes (`ready`, decommission fields, hosted counts).
3. **`settings_status: parse-error`** (Clusters list): today's typed `kube` client fails a GET
   closed (a 5xx) rather than returning a partially-parsed object, so "failed to parse" as the spec
   describes it cannot currently occur as a distinct state from "absent" or "present." Resolved by
   keeping the variant in the wire type for forward compatibility (a future looser read path could
   populate it) but noting today it is unreachable, rather than building a lenient parse path this
   plan has no other need for.
4. **Owners list "every owner with a Quota object or any live object"** (spec's Owners screen)
   widens today's `usage_all`, whose own doc comment says it lists only owners with a Quota or a
   request. Resolved in Task 3 Step 2 by adding a third enumeration source (owners named in
   `Workspace`/`Environment`/`Volume` `spec.owner`, never the label) rather than reusing the
   narrower existing list, since the spec's screen explicitly needs "every owner... or any live
   object."
