# Snapshot state: freezing the definition with the bytes

**Date:** 2026-09-03
**Status:** approved (owner, 2026-09-03), ready for planning

## Problem

A `Snapshot` records provenance only: volume, owner, worktree, parent, message, the pinned and
transient flags, and a phase. The bytes under `{pool}/vol/<id>/snap/<name>` are the workspace's
files. Nothing records what the workspace or environment WAS when the cut was taken — its image,
its package list, its resources, an environment's services. Two places already pretend otherwise:
`Workspace.live_state` in the API doc ("the Snapshot record's state at push time") is always
`null`, and every history row answers `"state": null`.

The consequence shows at restore: a workspace restored from a month-old snapshot gets last month's
files with today's image and package list — or, when the source is gone, the defaults. An
environment restored from a snapshot gets its data back and no services at all; `restore_env`
says so in its own comment.

## Decision

Every cut freezes the parent's definition beside the bytes, in the `Snapshot` CR itself, and
restore uses it as the default. Nothing new is stored anywhere else.

## The record

`SnapshotSpec` gains one optional field:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub state: Option<SnapshotState>,
```

```rust
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SnapshotState {
    Workspace {
        image: String,
        packages: Vec<String>,
        resources: PodResources,
        quota_gb: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attached_environment: Option<String>,
    },
    Environment {
        services: Vec<crate::model::Service>,
        quota_gb: u64,
    },
}
```

Serialized with `kind: "workspace"` / `kind: "environment"` as the tag, camelCase fields
(`quotaGb`, `attachedEnvironment`), so the CRD schema, the history JSON and the web all read the
same shape. `PodResources` and `model::Service` are the existing types; the state is a copy of
spec fields, never a new vocabulary.

Two derivations, one per kind, and every cutter calls one of them:

```rust
impl SnapshotState {
    pub fn of_workspace(w: &Workspace) -> Self;    // image, packages, resources, storage.quota_gb, attached_environment
    pub fn of_environment(e: &Environment) -> Self; // services, storage.quota_gb
}
```

`quota_gb` comes from `spec.storage.quota_gb`, or the kind's default quota when `storage` is
`None` (the same fallback `/v1` applies at create).

## Who writes it

Whoever cuts the snapshot, from the parent's spec at that instant. There are five cut sites and
all five stamp `state`:

| cut | cutter | source of the parent spec |
|---|---|---|
| push (`POST …/push`) | `/v1` | the Workspace/Environment it just read to authorize the push |
| clone cut (`clone-{ws}-{hex}`) | `/v1` | the source Workspace (`src`) |
| stop cut (`stop-{parent}-{gen}`) | agent, `controller/stop.rs` | the parent object the reconcile holds |
| sync cut (`sync-{ws}-{hex}`) | agent, `sync.rs` | `listing::Parent.state` |
| migration baseline | agent, `controller/workspace.rs` | the parent object the reconcile holds |

`listing::Parent` gains `pub state: SnapshotState`, derived while the listing already
deserializes the full object — no extra read anywhere. Sync cuts carry state for one reason: an
interrupted clone seeds from the newest held sync cut (`VolumeSource::SeededFrom`), and that
clone should come up as the workspace was, not as it is now.

`/v1`'s two cut sites already hold the parent; the agent's three hold either the object or the
`Parent`. A cut site that cannot produce a state does not exist after this change; `state` is
`Option` only so that snapshots written before the change still deserialize.

## Who reads it

**Restore** — `POST /v1/workspaces/restore` and `POST /v1/environments/restore` resolve the
snapshot first (they already do), then build the new object's spec by precedence:

1. an explicit field in the request body;
2. the snapshot's `state`;
3. today's fallback: the live source's spec when it still exists, else the kind's defaults.

`RestoreBody` (workspace) gains optional `image`, `packages`, `resources`, `quota_gb`,
`attached_environment`; `RestoreEnvBody.services` becomes `Option<Vec<Service>>` (absent ⇒ the
snapshot's services; `[]` explicitly ⇒ no services, today's "data only" restore). Every value that
reaches the spec passes the same validation create applies (`check_services`, package and image
checks) whether it came from the body or from the snapshot — a snapshot written by an older
build is data, not an authorization.

**Clone** is unchanged: it copies a live object and already reads that object's spec. The one
exception is already covered by the table above: a clone of an interrupted workspace seeds from a
sync cut, and the seeded clone's spec is still taken from the source Workspace CR (readable even
when its node is dead), so it needs nothing from the snapshot.

**History** — `commit_model_history_rows` fills `"state"` with the snapshot's state (or `null`
for a pre-change snapshot). `GET …/history` and `GET …/refs` therefore expose it with no route
change. `Workspace.live_state` in the doc stays `null` and its comment is corrected to say that
the per-snapshot state lives on the history rows; the field is not removed in this change (the
web types name it).

**Web** — the history list renders a one-line summary under each row: `alpine:3.20 · 4 packages`
for a workspace, `3 services` for an environment, nothing for a row without state. The restore
dialog pre-fills its editable fields from the selected row's state so the person sees what they
are restoring and can change it before submitting.

## Rules

- **The CR is the record.** No object store, no sidecar file. A Snapshot with a state is bounded
  by the same 1 MiB etcd limit as the Environment spec it copies; an environment whose spec fits
  produces snapshots that fit.
- **Copy, never reference.** The state is a value frozen at cut time. Later edits to the parent
  never change an existing snapshot's state.
- **Data, not authorization.** A restore validates everything it takes from a snapshot exactly as
  it validates a request body. Owner and team come from the caller and the source, never from the
  state.
- **Absent means old.** `state: None` is only ever a snapshot cut by a build before this change.
  Every reader has a fallback for it; no reader errors on it.
- **No new exposure.** Service environment variables are already stored in the Environment CR
  and returned by `GET /v1/environments/{id}`; copying them into the Snapshot CR, read through the
  same owner-scoped `/v1` paths, adds no new reader.

## Cases checked

| case | behaviour |
|---|---|
| restore a workspace whose source was deleted, snapshot has state | image, packages, resources, quota, attachment from the snapshot; team empty, region from the request or `default` as today |
| restore an environment with no `services` in the body, snapshot has state | the snapshot's services, validated by `check_services` |
| restore with `services: []` explicitly | no services (today's data-only restore) |
| restore from a pre-change snapshot, source alive | today's behaviour (source spec) |
| restore from a pre-change snapshot, source gone | today's behaviour (defaults) |
| clone of an interrupted workspace | seeds bytes from the held sync cut; spec from the source Workspace CR; the sync cut's state is present but unused here |
| package or image in a snapshot fails today's validation | the restore is refused with the same 4xx create would give; the person overrides in the body |
| snapshot state names an attached environment that no longer exists | restore drops the attachment (same as `/v1` does when an attachment target is missing at create) |

## Out of scope

- Diffing states between snapshots, or showing a per-snapshot "what changed".
- Storing anything outside the Snapshot CR.
- Removing `Workspace.live_state` from the API doc (web types name it; a later cleanup).
- Cross-region restore (unchanged from the 2026-09-02 design).

## Testing

- `crd.rs`: `SnapshotState` round-trips through JSON with the camelCase/tag shape; a
  `SnapshotSpec` without `state` deserializes; `deploy/k3s/crds.yaml` regenerated and the drift
  test passes.
- Each of the five cut sites: the created Snapshot carries the state derived from the parent
  (recorded-request assertions on the POST body).
- Restore: the four precedence cases in the table above for both kinds, plus the validation
  refusal.
- History: a row carries `state` when present and `null` when absent.
- Web: `bun test` for the summary line; `tsc` and `lint` clean.
- Live: push, restore from that snapshot with the source deleted, and confirm the new workspace's
  image and packages equal the snapshot's; same for an environment with two services.
