# Review: `crates/workspaces` + `bins/api`

**Commit:** master `4c7e94c9`
**Scope:** every file under `crates/workspaces/src` (`crd.rs`, `api.rs`, `model.rs`, `engine/*`,
`packages.rs`, `k8s.rs`, `kube_test.rs`, `replicate.rs`, `store.rs`, `cosmos.rs`, `upstream.rs`,
`registry.rs`) plus `bins/api/src/main.rs`, read against `CLAUDE.md`,
`2026-09-03-durable-snapshots-design.md` and `2026-09-03-snapshot-state-design.md`.
**Method:** read only. No edits, no builds.

**Counts:** Critical 2 · Important 7 · Minor 10 · Cleanup 12

---

## Critical

### C1 — `delete_volume` / `delete_snapshot` delete another owner's snapshots

`crates/workspaces/src/api.rs:2264` (`delete_volume`), `:2306` (`delete_snapshot`),
`:2366` (`commit_model_snapshots_maybe_empty`).

`commit_model_snapshots_maybe_empty` lists `Snapshot`s by `spec.volume` and then **filters the list
down to the caller's own owner set**:

```rust
let mut items: Vec<crd::Snapshot> =
    list.items.into_iter().filter(|sn| owners.contains(&sn.spec.owner)).collect();
```

Both delete paths then reason about the volume using only that filtered view:

* `delete_volume:2272` calls `commit_model_snapshots` purely as an ownership *probe* — "at least one
  snapshot here is readable by me" — and then at `:2278` deletes the **whole `Volume` CR**, which
  cascades to every `Snapshot` on it (they are its children, by design rule 3) and every subvolume
  the agent's byte sweep then reclaims.
* `delete_snapshot:2338-2346` computes `remaining` over the same owner-filtered list. Another
  owner's snapshots on the volume are invisible to that predicate, so `remaining == false` and the
  handler deletes the Volume CR out from under them.

A volume genuinely holds snapshots from more than one owner: `restore_ws` (`:1420`) writes
`CloneOf { volume, commit }` naming the **snapshot's** volume — which may be a team's environment
volume — while the new `Workspace.spec.owner` is the caller. `create_snapshot` (`:1927`) stamps the
snapshot with the *pushing worktree's* owner, not the volume's. So one push by a team member onto a
team volume, followed by that member deleting their own snapshot (or the volume), takes the team's
entire history with it. `parents_of_volume` is deliberately cluster-wide for exactly this reason and
its comment names the case; the snapshot listing never got the same treatment. The existing test
`a_foreign_worktree_on_the_volume_refuses_both_deletes`
(`crates/workspaces/tests/api_volumes.rs:295`) covers a foreign *worktree* but not a foreign
*snapshot*.

**Fix:** split the two questions the way `parents_of_volume` already does. Keep the owner-filtered
list for "may this caller see this volume at all" and for `/history`, `/refs`, and picking the delete
target; add an **unfiltered** `spec.volume`-selected count for the two decisions that destroy other
people's data:

* `delete_volume` — refuse (409) when any snapshot on the volume has an owner outside
  `caller_owners`, rather than deleting all of them.
* `delete_snapshot` — compute `remaining` from the unfiltered list, so a volume still holding
  someone else's snapshot is never collected.

Add the mirror of `api_volumes.rs:295` for a foreign snapshot in both handlers.

### C2 — Any authenticated caller can create unbounded workspaces and environments

`crates/workspaces/src/api.rs:538` (`create_ws`), `:1502` (`create_env`), `:1208` (`clone_ws`),
`:1330` (`restore_ws`), `:1567` (`restore_env`); `crates/workspaces/src/k8s.rs:860` (`ws_namespace`'s
doc); `crates/api/src/lib.rs:155-158`.

There is no per-owner limit anywhere on the number of workspaces, environments, volumes or
snapshots, and no rate limiter on any `/v1/workspaces*` or `/v1/environments*` route (the sibling
`/v1/signin/email` and `/v1/cli/code` routes both carry `ratelimit` layers, so the absence here is
asymmetric rather than a platform-wide choice). `clamp_quota` (`:534`) caps a *single* volume at
500 GB and `PodResources::default()` reserves 4 GiB / 2 vCPU per pod, but nothing caps the count.

`crd.rs:860` states the design intent explicitly — "it gives a per-user `ResourceQuota` somewhere to
live, which is the unit a limit is naturally expressed in" — and `grep -rn ResourceQuota crates bins`
returns **only that comment**. `k8s::limit_range` (`:138`) creates a `LimitRange`, which bounds
per-container size and not aggregate consumption.

A `for` loop over `POST /v1/workspaces` therefore reserves the cluster's whole schedulable memory
and fills the btrfs pool with 20 GB qgroups, from a single ordinary account. Each create also spawns
a detached 5-second polling task (`install_user_key_after_placed`, `:602`), so the same loop leaves
unbounded tokio tasks on the api pod.

**Fix, cheapest first:**

1. A count check in `create_ws`/`create_env`/`clone_ws`/`restore_ws`/`restore_env`: the listings are
   already label-selected and one `owned_in`/`owned_by` list per create is the same cost
   `refuse_taken_name` already pays — refuse with 429/409 past `WS_MAX_PER_OWNER`
   (env-configurable, default e.g. 20).
2. Have the workspace-namespace reconciler emit a `ResourceQuota` alongside the existing
   `LimitRange`, which is the durable backstop for anything written by a path other than `/v1`.

---

## Important

### I1 — `ssh_session`'s name fallback authorizes on a label, then mints a token

`crates/workspaces/src/api.rs:796-815`.

```rust
Err(_) => {
    api.list(&owned_by(&owner)).await?.items.into_iter()
        .find(|w| w.spec.name == target).ok_or_else(not_found)?
}
```

`owned_by` is `OWNER_LABEL={owner}` — a label selector, with no `w.spec.owner == owner` recheck
afterwards (contrast `refuse_taken_name:331` and `my_ws:762`, which both do recheck). The handler
then calls `s.jwt.mint_ssh_session(&owner, &id, region)` and returns the host key. CLAUDE.md's rule
is unambiguous: *"Never authorize on a label; `may_act_on` reads `spec.owner`."* Any object whose
owner label disagrees with its `spec.owner` — a restored backup, a migration, an operator with
kubectl, or the window before the agent's `heal_labels` re-stamps it — becomes an ssh session into
someone else's workspace.

**Fix:** add `.filter(|w| w.spec.owner == owner)` before `.find(...)`, exactly as
`refuse_taken_name` does. One line.

### I2 — `list_ws`, `list_volumes` and `pushed_volumes` all authorize on labels

`crates/workspaces/src/api.rs:741` (`list_ws`), `:2184` (`list_volumes`), `:379` (`pushed_volumes`
fallback), `:1683` (`list_env`), `:2092` (`live_parents`).

Same class as I1 and the same one-line shape of fix. Each selects by `OWNER_LABEL` /
`owner_set_selector` and builds the response straight from the returned items with no `spec.owner`
recheck. The delete/history paths (`commit_model_snapshots_maybe_empty:2374`,
`find_commit_model_snapshot:2397`) *do* recheck, which makes the inconsistency accidental rather
than deliberate. `list_volumes` is the worst of these because it also derives the row's
`volume: vol/{owner}/{name}` from `rows.first().spec.owner` (`:2213`) — a mislabelled snapshot both
appears in the wrong person's list and mislabels who owns it.

**Fix:** one `.filter()` per listing against the owner set the handler already computed
(`owner`/`owners`/`caller_owners`). Nothing else changes; the label selector stays as the index.

### I3 — `delete_snapshot`'s running-base check only reads `status.head`

`crates/workspaces/src/api.rs:2320-2325`, with `Parent.head` from `parents_of_volume:2122`.

The refusal is `live.iter().any(|p| p.head.as_deref() == Some(snapshot))`. `status.head` is written
*only by the node actually running the pod* (`crd.rs:646`), so a workspace or environment created by
`clone_ws`/`restore_ws`/`restore_env` with
`storage.source = CloneOf { commit: Some(id) }` has `head == None` for the whole interval between the
`/v1` create and the owning node's first successful checkout — minutes on a cold node, indefinitely
while its node is down. During that window the snapshot its checkout depends on is deletable. The
result is a permanent failure: `engine::checkout` (`engine/commit.rs:57`) answers `NO_SUCH_RECORD`,
which `bins/agent/src/controller/volume.rs:224` classifies as permanent and never retries.

**Fix:** also refuse when the snapshot is named by any live parent's
`spec.storage.source` — `CloneOf { commit: Some(id) }` or `SeededFrom { snapshot: id }`.
`parents_of_volume` already holds the whole object, so this is one more field on `Parent` and one
more clause in the `any`. Same clause belongs in `delete_volume`'s emptiness check.

### I4 — `GET /v1/regions` hands every authenticated caller the Azure storage account names

`crates/workspaces/src/api.rs:274-281`; `crates/workspaces/src/model.rs:8-15`.

`list_regions` is gated only by `caller()` (any signed-in user) and returns the whole `Region`
struct, including `storage_account` and `blob_container`. Per the durable-snapshots design,
*"Snapshot BYTES have no object store at all"* — those two fields are dead weight from the
pre-cutover era, and publishing them is a free infrastructure-topology disclosure to every user.

**Fix:** return a projection with `id`, `name`, `status` only (the three fields `check_region` and
the web actually consume), and delete the two fields from `model::Region` in the same pass — see
CL7.

### I5 — `parents_of_volume` runs two unfiltered cluster-wide LISTs per delete request

`crates/workspaces/src/api.rs:2122-2139`.

```rust
for w in Api::<crd::Workspace>::all(c).list(&ListParams::default()).await.ok()?.items { ... }
for e in Api::<crd::Environment>::all(c).list(&ListParams::default()).await.ok()?.items { ... }
```

Every `Workspace` and `Environment` in the cluster, deserialized in full, twice per
`delete_snapshot` (it is called at `:2320` **and** again at `:2339`) and once per `delete_volume` —
so a snapshot delete is four full-cluster lists. The unfiltered listing is correct (the comment
explains why an owner-scoped one has a blind spot); the *cost* is what needs fixing.

**Fix:** add `selectable = ".status.volumeRef"` to `Workspace`/`Environment` in `crd.rs` (both
already carry `selectable = ".status.nodeName"`, so the mechanism and its "apply the CRDs before
rolling" caveat are established) and select on it here. That also collapses the two calls in
`delete_snapshot` to a cheap indexed read, so the deliberate re-read at `:2339` stays affordable.
Interim, zero-schema fix: pass the first call's result into the second decision rather than listing
again, and drop the second list to a single re-read of the snapshot count.

### I6 — `restore_ws` accepts an environment snapshot; neither restore checks the state's kind

`crates/workspaces/src/api.rs:1345` (`restore_ws`), `:1356-1361`, `:1590` (`restore_env`).

`find_commit_model_snapshot_for_restore` checks existence, `Ready`, and owner — not what kind of
thing the snapshot was cut from. `restore_ws` then matches
`Some(SnapshotState::Workspace { .. })` and falls to `_ => None` for an `Environment` state, so
restoring an environment snapshot silently produces a workspace with the *default* image, the
default quota and no packages, mounting a database's data directory. It is not a privilege
escalation — `caller_owners` gates readability either way, and `restore_env:1597` explicitly
sanctions "restores under its own owner, or under you" — but it is a request the API should refuse
rather than half-honour.

**Fix:** after resolving the snapshot, refuse with 400 when `spec.state` is present and its kind
does not match the route (`"this snapshot was cut from an environment; use POST
/v1/environments/restore"`). A `state: None` legacy snapshot keeps today's behaviour, per the
"absent means old" rule.

### I7 — `api.rs` is 2 659 lines with five resources in one file

`crates/workspaces/src/api.rs`.

It holds regions, workspaces, environments, push, volumes/snapshots, the kube helpers, the label
helpers, the `Directory` trait and 190 lines of tests. Concretely, this is what let I1–I3 drift:
the same "list then filter by `spec.owner`" rule is written correctly in three places
(`refuse_taken_name`, `my_ws`, `commit_model_snapshots_maybe_empty`) and omitted in four
(`list_ws`, `list_volumes`, `pushed_volumes`, `ssh_session`), and the split-brain in C1 comes from
two helpers with near-identical names disagreeing about whether "the snapshots on this volume" means
the caller's or everyone's.

**Fix — a mechanical split, no behaviour change:**

```
api/mod.rs        ApiState, Directory, router(), caller/unauthorized/kube_err/store_err
api/scope.rs      caller_owners, may_act_on, teams_for, owned_by/owned_in, owner_set_selector,
                  my_ws, find_env, and ONE `mine<K>(items, owners)` filter every listing calls
api/workspaces.rs create/list/get/delete/start/stop/attach/detach/packages/clone/restore/ssh
api/environments.rs the environment twins + restore_env_in_place
api/volumes.rs    list_volumes, history, refs, delete_volume, delete_snapshot, snapshot_rows
api/push.rs       push_ws, push_env, create_snapshot, clone_base, refuse_cut_in_flight
```

The load-bearing part is `scope.rs`: once "narrow by label, decide on `spec.owner`" is one function,
I1 and I2 cannot recur, and C1 becomes a visible choice between two named helpers rather than an
invisible one.

---

## Minor

* **M1 — `attach_ws` patches an unvalidated id into a label value.**
  `api.rs:979-983` puts `body.environment` into `metadata.labels[ATTACHED_ENV_LABEL]` verbatim.
  `find_env` having succeeded means the value is a legal object name, so this is not exploitable
  today, but the guarantee is incidental. `model::validate_ws_spec:232` applies exactly the right
  check at the agent — call the same `valid_segment(env) && env.len() <= 63` here so a bad value is
  a 422 rather than a kube 422 laundered into a 500 by `kube_err`.

* **M2 — `create_ws`/`list_ws` compare team membership case-sensitively, then lowercase.**
  `api.rs:547-553` and `:734-738`: `may_act_on(&owner, t)` runs on the raw string and only the
  accepted value is `to_lowercase()`d. A caller passing `Acme` for a directory slug `acme` gets a
  404 "no such team". Lowercase before the membership check.

* **M3 — Mutation responses always report `volume: null`.**
  `stop_ws:940`, `start_env:1724`, `stop_env:1735`, `delete_env:1779`, `clone_ws:1261`,
  `patch_ws_packages:1051`, `restore_ws:1429` all build the doc with `&HashSet::new()`, so
  `ws_doc`'s `volume` field (`:403`) is `None` even for a volume with fifty pushes. A web client
  that treats `volume == null` as "never pushed" gets a wrong answer from every one of these.
  Either pass the real pushed set (one call, already available) or document the field as
  read-only-on-GET.

* **M4 — `live_parents` keeps one parent per volume.**
  `api.rs:2100`/`:2106` `insert` into a `BTreeMap` keyed by volume, so a volume carrying two
  worktrees (a shared clone, a restore) shows only the last one's `display_name`/`kind` in
  `list_volumes`, and an environment inserted after a workspace on the same volume overwrites it.
  Cosmetic in the listing; note that the *delete* paths correctly use `parents_of_volume`, which
  returns a `Vec`.

* **M5 — `restore_ws` treats a volume name as a workspace id.**
  `api.rs:1349`: `my_ws(&s, &owner, &volume)`. It works because an owned volume shares its parent's
  id, but it is a coincidence of the naming scheme, not a lookup. For a shared-clone volume it
  resolves the *source* workspace and inherits its team and region. Resolve the parent from
  `status.volumeRef` (as `parents_of_volume` does) or state the assumption in a comment.

* **M6 — `swap_worktree` leaves `-restoring` / `-before-restore` siblings under `live/`.**
  `engine/commit.rs:78,87`. Those paths are worktree-shaped, so `set_quota_worktrees`
  (`engine/ops.rs:195`) qgroup-limits them and any `read_dir` of `live/` counts them as worktrees.
  A crash between the renames leaves one behind indefinitely. Give them a prefix the worktree
  scanners skip (`.restoring-{ws}`), or sweep them in the janitor.

* **M7 — `optional_push_message` is `async` and awaits nothing.**
  `api.rs:1849`. Drop `async` and the two `.await`s at `:1866`/`:1881`.

* **M8 — Stale comments naming the pre-cutover shape.**
  `k8s.rs:1024` "One **Deployment** per service" on a function that builds a `StatefulSet`;
  `api.rs:1888` "the agent's `reconcile_commit`"; `api.rs:1816` "`CommitPending` guard";
  `engine/commit.rs:103` "not a registry read" describing a registry that no longer exists;
  `api.rs:2016-2013` the `// ── volumes ──` block header still says *"The index and the records both
  live on the SERVER tier"*, which `list_volumes:2180` explicitly contradicts two screens later.

* **M9 — `snapshot_rows` emits two permanently-constant fields.**
  `api.rs:2430-2431`: `"lineage": []` and `"region": ""` on every row, carried over from
  `registry::CommitRecord`. Keep them only if a web type still names them; if it does, say so in the
  comment the way `live_state` does.

* **M10 — `delete_env` and `refresh_user_keys` swallow partial failures with only a warn.**
  `api.rs:1772` and `:663`. Correct as a policy (both are documented as best-effort), but neither
  reports anything to the caller, so a `DELETE /v1/environments/{id}` that left ten workspaces
  pointing at a dead environment answers a clean 202. Consider a `warning` key on the body, as
  `stop_ws:944` already does for the node-dead case.

---

## Cleanup

* **CL1 — `Upstream::history` is dead.** `upstream.rs:81-88`. Nothing in the workspace calls it
  (the only `Upstream` use left is `pushed_volumes:366` → `volumes`). Delete it.
* **CL2 — `upstream::Provenance` and its test are dead.** `upstream.rs:92-134`. Its whole job —
  "what the volume belonged to at push time" — is `crd::SnapshotState` now. Delete the struct, the
  `impl`, and `provenance_reads_past_unrelated_state_and_tolerates_none`.
* **CL3 — `VolumeRow.latest_ms` is never read.** `upstream.rs:26`; `pushed_volumes:372` maps only
  `row.name`.
* **CL4 — Five `VolExt` methods have no caller.** `registry.rs:95-104`: `append_commits`,
  `move_ref`, `ref_commit`, `commit`, `region`. Only `vol_exists`, `history` and
  `volume_marker_prefix` are used, by the deliberately FROZEN
  `bins/server/src/browse_api/volumes.rs`. Trim the trait to those three; `volume_marker` /
  `REGION_KEY` / `ref_key` / `commit_key` go with them. (Keep the read half — `volumes.rs:3-9`
  documents the keep-until-drained ruling.)
* **CL5 — `pushed_volumes`' entire purpose is one display field.** `api.rs:364-388`. It costs an
  HTTP round trip to the peer listener (or a cluster-wide `Snapshot` list) per `list_ws`/`get_ws`,
  and per *owner* in `list_env:1682`, solely to decide whether `ws_doc.volume` is `Some`. Now that
  `list_volumes` is CRD-backed, the same answer is already in the snapshot listing the Snapshots
  page makes. Delete `pushed_volumes` and `ApiState::upstream`/`with_upstream` with it, or state
  in the comment why the round trip is worth one nullable string.
* **CL6 — `Workspace.live_state` is permanently `null`.** `model.rs:79-84`, `api.rs:409`. Kept only
  because `web/apps/web/src/lib/api.ts:709` names it. The snapshot-state design lists removing it
  as out of scope; add it to a web-side cleanup ticket so the field does not outlive the reason.
* **CL7 — `Region.storage_account` / `blob_container` are dead fields.** `model.rs:12-13`,
  `api.rs:266-267`. No snapshot byte has touched an object store since the durable-snapshots
  cutover. Removing them also fixes I4.
* **CL8 — Two near-identical snapshot resolvers.** `find_commit_model_snapshot:2386` and
  `find_commit_model_snapshot_for_restore:2406` differ only by the `snap.spec.volume != volume`
  clause. One function taking `Option<&str>` for the volume.
* **CL9 — Two copies of the one-cut-in-flight guard.** `refuse_cut_in_flight:1096` and the inline
  `racing` block in `create_snapshot:1905-1920`, including the same 409 string. Call the helper from
  `create_snapshot`.
* **CL10 — Vocabulary drift against the approved design.** The design is explicit that *"There is no
  'commit'"*. The crate still has `commit_model_snapshots`, `commit_model_snapshots_maybe_empty`,
  `find_commit_model_snapshot*`, `crd::commit_labels`, `engine/commit.rs`, `commit_worktree`,
  `local_commits`, `drop_commit`, `VolumeSource::CloneOf { commit }` and `RestoreWish` prose about
  "commits". The `CloneOf.commit` **field name is on stored CRs** and must stay (or move behind a
  serde alias); everything else is internal and renameable to `snapshot`. Doing it in one mechanical
  pass is cheaper than the drift.
* **CL11 — Sixteen `Task N` references in shipped comments.** `api.rs:1304,1338,1342,1587,2304,2333`,
  `crd.rs:89,243`, `engine/ops.rs:19,168`, `engine/commit.rs:149`, `registry.rs:30`,
  `k8s.rs:1511,1549,1560`. The plans they name are not in the tree; each should either state the
  fact without the task number or cite the design doc.
* **CL12 — `crd.rs` is 1 260 lines and carries `Snapshot`, `Volume`, `VolumeReplica`, `Workspace`,
  `Environment`, `OwnerBinding`, the namespace/name helpers and 240 lines of tests.** Lower priority
  than I7 (it is declarations, not logic), but the naming helpers (`ws_namespace`, `env_namespace`,
  `binding_name`, `dns_label`, `pair_tail`) are a self-contained unit that would read better as
  `crd/names.rs`.

---

## What is good and should not be touched

1. **`model::validate_mount` / `validate_service` / `valid_ws_name`, and the fact each is re-checked
   at the agent** (`k8s::git_init_container:798`, `k8s::workspace_pod:863`,
   `k8s::service_statefulset:1043`, `model::validate_ws_spec`). Trust boundary enforced at both ends,
   with the *reason* in the comment and a test naming the actual payload
   (`create_env_refuses_a_traversing_mount`). This is the strongest code in the crate.
2. **`packages.rs`.** Pure, no I/O, a grammar that cannot escape into a Nix expression, and
   `the_expression_does_not_depend_on_which_workspace_asked` locking in the property that makes
   profile reuse work. Do not add a dependency to it.
3. **The `parents_of_volume` vs `live_parents` split and its `None`-means-unanswered discipline**
   (`api.rs:2080-2085`, `:2119-2121`, `kube_unavailable:2282`). Opposite biases for listing and for
   deleting, both stated, both correct. C1 is a gap *next to* this reasoning, not a flaw in it.
4. **`crd::lenient_state` + `preserve_unknown_state`** (`crd.rs:255-296`). A hand-written permissive
   schema plus a warn-and-drop deserializer, with the kube-core panic that forced it written down.
   Exactly the right trade, and `a_malformed_snapshot_state_is_dropped_not_a_deserialize_error`
   holds it.
5. **`crd::Snapshot::is_snapshot` and its shape-matched legacy baseline**, with a `ponytail:` marker
   naming the exact false positive and the upgrade path (`crd.rs:428-438`). One predicate instead of
   a migration job.
6. **`kube_test::mock_client`'s `sent()` recorder.** Asserting on the JSON body written to the API
   server is the only way to test these handlers, and the ordered-replay behaviour for repeated
   method+path is what makes the conflict-adopt flows testable at all.
7. **`crd::ws_namespace`'s hashed team tail** and `owners_namespaces` recomputing names rather than
   parsing them back out (`api.rs:685`), with
   `a_dns_truncated_team_namespace_is_still_the_owners` pinning the case the old `ends_with`
   heuristic dropped.
8. **`replicate::targets`** — rendezvous hashing on sha2 with the "not `DefaultHasher`" reason
   stated, and `adding_a_node_moves_few_volumes` proving the property that justifies it.
9. **The error hygiene**: `store_err`/`kube_err`/`upstream_err` all log the detail and answer a fixed
   string, held by `backend_error_text_never_reaches_the_caller` (`api.rs:2476`). `Upstream`'s
   module doc keeps the peer secret out of every error. Keep this discipline in any new handler.
10. **`k8s::hardened()` and the NetworkPolicy generators.** The capability add-back list is justified
    by an observed failure, not theory, and the policies are generated so there is one definition of
    the isolation rule — with tests that assert the metadata service and RFC-1918 are excluded.
