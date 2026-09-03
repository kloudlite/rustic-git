# Live settings, managed from the superadmin

**Date:** 2026-09-03
**Status:** approved by the owner in outline ("option 1": runtime tunables only; infrastructure
stays read-only in the admin area). Effect timing: next beat, at most one minute.

## Problem

Every tunable is an environment variable in a deploy yaml. Changing one is a commit, a roll and a
pod restart, and nobody can see the current value without reading the manifest. The owner wants
every setting of the central servers and of each cluster edited from the superadmin area
(`2026-09-03-quotas-and-superadmin-design.md` §5), with the same audit and access rules.

The inventory (`docs/superpowers/reviews/2026-09-03-tunables-inventory.md`) found 93 env knobs
across six binaries: 22 secrets, ~60 bootstrap-only (listen addresses, store URLs, pool paths,
node identity, image refs), and the rest tunables that a running process never re-reads because
each is captured once into a struct or a `OnceLock`.

## Decisions

### 1. Two scopes, one shape

| scope | object | written by | read by |
|---|---|---|---|
| **central** | one document at object-store key `cluster/settings` (JSON) | the admin server, through a new peer-only route on the server tier (`PUT /api/admin/settings`, peer listener, superadmin JWT + peer secret) | `rustic-git` (server), `rustic-git-worker`, `rustic-git-gateway`, `rustic-git-api` (user role), web |
| **cluster** (one per region) | a cluster-scoped `ClusterSettings` CR named `default` in that region's k3s | the admin server, through its region kube client | every `rustic-git-agent` in that cluster, and `rustic-git-api` for the per-owner cap |

Both scopes are the same Rust type family: a `Settings` struct per tier with `serde(default)` on
every field, so a missing key means "the built-in default". The env var of each knob stays as the
**bootstrap default**: `Settings::from_env()` seeds the defaults, the stored document overrides
field by field, and the admin UI shows all three columns (default, env, stored).

### 2. Which knobs

Every non-secret knob that is not process identity. Secrets never; listen addresses, store URLs,
pool paths, node names and region identity never (they are what makes a process the process it
is). Everything else is a setting, and each is marked one of two ways in code:

- **live** — read from the handle on every use or at the top of every beat; a change lands on the
  next beat, no restart.
- **boot** — read once at start (images for tenant pods such as `WS_DEFAULT_IMAGE` and
  `WS_GIT_INIT_IMAGE`, `WS_RUNTIME_CLASS`, the git SSH host/port the init container clones from,
  log format, worker lane counts, and every knob the inventory marked "cached once" that a
  refactor to live would not pay for). A change to a boot setting is written the same way, and
  then the readers are ROLLED so they start with the new value (§7).

The UI shows the mark on every row, so the person saving knows whether pods will restart.

**Cluster (`ClusterSettings.spec`)** — the agent's beats and limits:
`syncSecs` (WS_SYNC_SECS), `replicaSecs` (WS_REPLICA_SECS), `decommissionSecs`
(WS_DECOMMISSION_SECS), `nodeDeadSecs` (WS_NODE_DEAD_SECS), `peerSendTimeoutSecs`,
`peerServeTimeoutSecs`, `peerReceiveSlack` (WS_PEER_RECEIVE_SLACK), `stopFlushTimeoutSecs`,
`nixTimeoutSecs` (WS_NIX_TIMEOUT), `nixpkgs` (WS_NIXPKGS), `basePackages` (WS_BASE_PACKAGES),
`defaultReplicas` (Volume.spec.replicas default), `maxPerOwner` (WS_MAX_PER_OWNER, until the Quota
CR replaces it), `homeCacheGb`, `quotaGbCeiling` (the `clamp_quota` 500).

**Central (`cluster/settings`)** — the server tier's limits and beats:
`maxBody`, `maxLayer`, `maxManifest`, `uploadGraceSecs`, `gcIntervalSecs`, `mergeLeaseSecs`,
`announceStrandedSecs`, `feedRetention`, `cloneHost`/`sshHost`/`sshPort`/`registryHost` (the web's
clone-menu display values), `signupOpen` (if the directory has such a flag), plus every
feature flag the inventory tagged as a flag.

The exact list is the inventory's "candidates" section plus every "cached once" tunable whose
only reason for being bootstrap-only is the capture; the plan names each with its struct field.

### 3. How a process reads them

One `LiveSettings<T>` handle per binary: `Arc<ArcSwap<T>>` (or `RwLock<Arc<T>>` — whichever is
already a dependency) loaded at boot from env, then refreshed by a beat every 30 s
(`SETTINGS_REFRESH_SECS`, itself bootstrap-only): the agent from a `ClusterSettings` watch (kube
reflector, so a change lands within seconds and the beat is only the fallback), the central
binaries from an object-store GET of `cluster/settings` (cheap, one small key). Every beat that
today captures a value at spawn (`spawn_pull`'s interval, `sync` beat, decommission beat,
`retire_pass`) reads `settings.load()` at the top of each iteration instead; a changed interval
takes effect on the next tick of the old interval — "next beat" — and never mid-sleep.

A knob that a running operation already holds (a `btrfs send` in flight with the old timeout)
keeps the old value until it finishes. Nothing is interrupted by a settings change.

### 4. Validation, audit, safety

- Every field has a type, a unit, a range (`syncSecs: 10..=3600`), and a one-sentence
  description in code (`#[doc]` on the struct field, exported to the UI as JSON schema via
  schemars — the same crate the CRDs already use). The admin server validates against the range
  before writing; a value outside it is a 422 naming the field and the range.
- Every write records who and when: the CR carries `rustic-git.io/updated-by` and `/updated-at`
  annotations; the central document carries `updatedBy`/`updatedAt` fields; the last ten
  versions are kept (CR: an annotation with the previous spec; document: `cluster/settings.N`).
  "Revert" in the UI writes the previous version.
- A stored document that fails to parse (a future field with a bad type, a hand edit) is logged
  and ignored in full — the process keeps its last good settings, never a half-applied one.
- RBAC: only the admin server's ServiceAccount has `create/patch` on `ClusterSettings`; agents
  `get/list/watch`. The central route is peer-only and superadmin-only. Admission: the
  `ClusterSettings` spec is not touched by the agent's ValidatingAdmissionPolicy (it never
  writes it).

### 5. The admin UI

`/admin/settings` with two tabs: **Central** and **Clusters** (one panel per region). Each knob is
a row: name, description, unit, current value (editable), env bootstrap value, built-in default,
range, last change (who, when). Save writes only the changed fields. A "pending" marker shows
until the next refresh reports the value applied (the agent writes `status.observedGeneration`
on `ClusterSettings`; the central binaries expose the loaded version on their `/healthz`).

### 6. Read-only infrastructure view (option 2 deferred)

The same area shows, read-only: image pins per tier (from the Deployments), replica counts,
ingress hosts, each node's decommission status. No writes. A later spec may add them.

### 7. A boot setting change rolls its readers (owner, 2026-09-03)

Every setting field declares its readers and its mark
(`#[settings(mark = "boot", readers = "agent")]`, surfaced in the schema). Saving a change:

1. The admin server validates and writes the document / CR (§4).
2. For every changed **boot** field it collects the readers, and rolls each one — the pod
   template annotation `rustic-git.io/restarted-at: <RFC 3339>`, exactly what
   `kubectl rollout restart` does, so Kubernetes uses the workload's own strategy (the server
   StatefulSet one pod at a time, the agent DaemonSet node by node, Deployments by surge). The
   admin server never deletes a pod. A **live** field rolls nothing.
3. The UI says what will happen BEFORE the save ("Save and roll: rustic-git-agent in
   centralindia-k3s"), and requires a second confirmation when the reader set includes the
   server StatefulSet, whose roll moves database ownership between nodes (see "Deploying" in
   CLAUDE.md).

A process reads its boot settings at start from the same `stored ?? env ?? default` chain, so a
rolled pod comes up with the new value and the manifest's env is only the fallback.

- **A fixed list, never a free name.** The readers a field may name are the known workloads:
  central `rustic-git-srv` (StatefulSet), `rustic-git-api`, `rustic-git-worker`,
  `rustic-git-gateway`, `rustic-git-web`, `rustic-git-admin`; per region `rustic-git-agent`
  (DaemonSet, `kube-system`) and the region gateway. `admin::workloads::KNOWN` is that list.
- **Manual roll too**: `POST /admin/workloads/{scope}/{name}/roll` with a required `reason`, for
  a rotated secret or a stuck process. Same mechanism, same list.
- **One roll in flight per workload**: a save whose readers are still rolling is a 409 naming
  them with ready/desired; the settings write is NOT made, so the document never runs ahead of
  the pods. `GET /admin/workloads` lists every known workload with image, ready/desired, last
  roll (who, when, reason or the setting that caused it) and rollout state.
- **Audit**: who, when and why go on the workload (`rustic-git.io/rolled-by`, `/rolled-at`,
  `/roll-reason`: a free reason, or `setting:<field>`) and into the admin audit log.
- **RBAC**: the admin ServiceAccount gains `get/list/patch` on exactly those Deployments and
  the StatefulSet in the central namespace, and on the agent DaemonSet in each region. Nothing
  wider: no pod delete, no manifest edits.

Not doing here: editing images of the first-party workloads themselves, replicas or ingress
(option 2, still deferred). Tenant pod images (`WS_DEFAULT_IMAGE`, `WS_GIT_INIT_IMAGE`) are
settings because they are what the agent hands to tenants, not what the agent runs as.

## Rules

- **Env is the bootstrap, the store is the truth, the built-in default is the floor.** A knob is
  read as `stored ?? env ?? default`, always in that order.
- **Next beat, never mid-operation.** No settings change interrupts running work.
- **Last good wins.** An unparsable settings document changes nothing.
- **A setting has a range or it is not a setting.** Unbounded knobs stay env-only.
- **Secrets and addresses are never settings.**
- **A boot setting change rolls its readers; a live one rolls nothing.** The UI says which
  before the save.
- **A roll is an annotation on a known workload, never a pod delete or a free name.**

## Cases

| case | behaviour |
|---|---|
| superadmin sets `syncSecs` 60 → 30 for region A | agents in A cut sync points every 30 s from their next beat; region B unchanged |
| `nodeDeadSecs` set to 5 | 422: below the floor 60 |
| the central document is corrupted by hand | every central binary logs once per refresh and keeps its last good values |
| a region's `ClusterSettings` object is deleted | agents fall back to env, then built-in defaults, on their next refresh |
| admin server down | nothing changes; agents keep reading the CR |
| `peerServeTimeoutSecs` lowered while a send is in flight | that send keeps its old deadline; the next one gets the new |
| the web's clone host changed | clone menus show the new host on the next page load |
| superadmin changes `defaultImage` (boot, reader: agent) for region A | CR written, then the agent DaemonSet in A rolls node by node; audit says `setting:defaultImage` |
| superadmin changes `syncSecs` (live) | CR written, nothing rolls, agents pick it up next beat |
| superadmin rolls `rustic-git-worker` manually with reason "rotate peer secret" | template annotation patched; Deployment surges; audit annotations written |
| a boot save while its reader is still rolling | 409 naming the workload with ready/desired; nothing written |
| roll requested while the previous roll is still progressing | 409 with ready/desired |
| roll of a name not in the list | 404 |

## Not doing

Per-owner or per-workspace overrides; secrets rotation; image pins, replicas, ingress or node
changes from the UI (read-only view only); a settings history beyond ten versions.

## Testing

Unit: `stored ?? env ?? default` precedence per tier; range validation per field; a corrupt
document keeps last good. Recorder: the agent watch applies a new interval on the next beat (a
fake clock); the admin server's write path (422 out of range, annotations written). Live: change
`syncSecs` on the k3s cluster from the UI and watch the cut cadence change within a minute.
