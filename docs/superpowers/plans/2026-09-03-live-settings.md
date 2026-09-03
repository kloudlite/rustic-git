# Live settings, managed from the superadmin — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move every non-secret, non-bootstrap tunable (central and per-cluster) out of deploy
yaml env vars and into two live, validated, audited documents — one `cluster/settings` object in
the shared object store, one `ClusterSettings` CR per region — edited from the superadmin area and
read by every process on a beat, so a knob change is a form submit, not a commit-roll-restart.

**Architecture:** A `Settings` struct per tier (`ClusterSettings` for the agent, `CentralSettings`
for server/worker/gateway/api), each `serde(default)`, each field documented with `schemars` for
the admin UI's JSON schema. `Settings::from_env()` seeds the built-in-default-then-env layer; the
stored document/CR overrides field by field (`stored ?? env ?? default`). Every process holds one
`LiveSettings<T> = Arc<ArcSwap<T>>`, loaded at boot and refreshed by a beat: the agent via a `kube`
reflector on `ClusterSettings` (seconds-fresh, `SETTINGS_REFRESH_SECS` only the fallback), the
central binaries via an object-store GET of `cluster/settings` every `SETTINGS_REFRESH_SECS` (no
reflector — there is nothing to watch on an object store). Every beat that today captures an
interval once at spawn re-reads `settings.load()` at the top of each iteration. Writes go through
one new peer-only route on the server tier (`PUT /api/admin/settings`, central) and the existing
per-region kube client the admin server (`api::admin`, from
`docs/superpowers/plans/2026-09-03-quotas-and-superadmin.md`) already holds (`ClusterSettings`,
cluster-scoped like every other CRD in `crates/workspaces/src/crd`). Both writes validate ranges,
stamp who/when, and keep a ten-version history for revert.

**Tech Stack:** Rust 2021, `arc-swap` 1.9 (already a workspace dependency — `Cargo.lock` pins
1.9.2 via `metrics-exporter-prometheus`, promoted to a direct `crates/core` dependency here),
`schemars` (already used by every CRD in `crates/workspaces/src/crd`), `kube`/`k8s-openapi`
`#[derive(CustomResource)]`, axum handlers, `crates/workspaces/src/kube_test.rs`'s recorder-backed
mock `kube::Client` for agent-side tests, Next.js app router + `bun test` for the web tab.

**Spec:** `docs/superpowers/specs/2026-09-03-live-settings-design.md` (binding — every range,
default and rule below is copied from it) and the knob list in
`docs/superpowers/reviews/2026-09-03-tunables-inventory.md`'s "Candidates for live settings"
section plus every "cached once"/"needs-restart" row the spec's §2 names by var name. Runs AFTER
`docs/superpowers/plans/2026-09-03-quotas-and-superadmin.md`: this plan consumes `api::admin`,
the `rustic-git-admin` Deployment/Service/Ingress/ServiceAccount, `RUSTIC_GIT_API_ROLE`,
`Claims.superadmin`/`Caller { name, superadmin }`, `require_admin`, and the admin-only ClusterRole
split (`deploy/k3s/agent-rbac.yaml`'s header table, `deploy/k3s/api-rbac.yaml`) by name; it does
not recreate any of them.

## Global Constraints

Copied verbatim from the spec's "Rules":

- **Env is the bootstrap, the store is the truth, the built-in default is the floor.** A knob is
  read as `stored ?? env ?? default`, always in that order.
- **Next beat, never mid-operation.** No settings change interrupts running work — a beat that
  captured an old value for an in-flight operation (a `btrfs send` under `WS_PEER_SEND_TIMEOUT_SECS`,
  an in-flight nix build) keeps it until that operation finishes.
- **Last good wins.** An unparsable stored document or CR spec changes nothing; the process keeps
  its last successfully-applied settings and logs once per refresh.
- **A setting has a range or it is not a setting.** Every field in `ClusterSettings`/
  `CentralSettings` carries a `#[validate(range(min = .., max = ..))]`-equivalent check in the
  admin write path; a value outside it is a 422 naming the field and the range. Unbounded knobs
  (`WS_NIXPKGS`'s pin string, `WS_GIT_SSH_HOST`) stay env-only and are NOT in either struct.
- **Secrets and process identity are never settings.** Nothing here touches
  `RUSTIC_GIT_PEER_SECRET`, `RUSTIC_GIT_JWT_SECRET`, any `AWS_*`/`AZURE_*`/`RUSTIC_GIT_S3_URL`,
  `RUSTIC_GIT_CACHE_DIR`, `RUSTIC_GIT_PEER_ADDR`/`_SVC`/`RUSTIC_GIT_SELF`,
  `WS_POOL`/`WS_REGION`/`NODE_NAME`/`WS_HOMES_EXPORT`/`WS_PEER_ADDR`, or any of the web's
  `AUTH_*`/`RESEND_*` vars — corrected from the plan's earlier draft, which also listed
  `WS_DEFAULT_IMAGE`/`WS_RUNTIME_CLASS`/`WS_GIT_INIT_IMAGE` here: the spec's §2 explicitly puts
  those three IN scope as boot settings (tenant pod images, not the agent's own identity — see
  the knob list below and CLAUDE.md's "Tenant pod images ... are settings because they are what
  the agent hands to tenants, not what the agent runs as"). What stays excluded is what makes a
  process the process it is: its listen address, its store, its node/region identity, its
  credentials.
- Commit subjects are imperative sentence case with no tool attribution. No task numbers in the
  subject.
- Comments explain WHY, never what; match the density of `bins/server/src/router/route.rs`.
- **A boot setting change rolls its readers; a live one rolls nothing** (spec §7). Every field
  carries a `mark` (`live` or `boot`) and, when boot, a `readers` list drawn from the fixed
  `admin::workloads::KNOWN` set. The save path is validate → write → roll (Task 5); a roll target
  still mid-rollout from a previous change is a 409 and the settings write does not happen.

## Which mechanism carries `mark`/`readers`: a const table, not an attribute macro

The spec says "surfaced in the JSON schema the admin UI reads" — it does not require the mark to
live as a `#[settings(...)]` derive attribute. A custom attribute macro earns its keep only when
many crates need to declare the same shape independently; here there are exactly two structs
(`ClusterSettingsSpec`, `CentralSettings`), both defined once, both already hand-written with
`schemars`/`serde` derives. Writing a proc-macro crate to parse `#[settings(mark = "boot", readers
= "agent")]` and inject it into the schema is a compile-time dependency, a new crate, and a macro
to debug for something a `match` on the field name already does in five lines. **Decision: a
const table, `SETTING_META: &[(&str, Mark, &[&str])]` per struct, checked at startup by a test
that every struct field name appears exactly once** — the smaller rung on the ladder, and the one
that keeps the mark next to the range table it already has to parallel (Task 1/2's range checks
are already a hand-written table, not a derive attribute; this is the same shape). If a THIRD
settings struct shows up later, promote to a macro then — not before.

```rust
// crates/core/src/settings.rs (and mirrored in crates/workspaces/src/settings.rs for
// ClusterSettingsSpec — two tables, not a shared generic one, because the two structs' field
// sets don't overlap and a shared type would need an enum big enough to name both)
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mark { Live, Boot }

/// (field name as it appears in the JSON schema / wire document, mark, readers if boot).
/// `readers` is empty for every `Live` row — checked by `settings_meta_is_exhaustive` below.
pub const CENTRAL_SETTING_META: &[(&str, Mark, &[&str])] = &[
    ("maxBody", Mark::Live, &[]),
    ("maxLayer", Mark::Live, &[]),
    ("maxManifest", Mark::Live, &[]),
    ("uploadGraceSecs", Mark::Live, &[]),
    ("gcIntervalSecs", Mark::Live, &[]),
    ("mergeLeaseSecs", Mark::Live, &[]),
    ("announceStrandedSecs", Mark::Live, &[]),
    ("feedRetentionSecs", Mark::Live, &[]),
    ("cloneHost", Mark::Live, &[]),
    ("sshHost", Mark::Boot, &["rustic-git-gateway"]),
    ("sshPort", Mark::Boot, &["rustic-git-gateway"]),
    ("registryHost", Mark::Live, &[]),
    ("signupOpen", Mark::Live, &[]),
    ("logFormat", Mark::Boot, &["rustic-git-srv", "rustic-git-api", "rustic-git-worker", "rustic-git-gateway", "rustic-git-admin"]),
    ("workerLanes", Mark::Boot, &["rustic-git-worker"]),
];
```

A unit test, `settings_meta_is_exhaustive`, builds the struct's `schemars` schema and asserts its
property names are exactly the const table's field names (set equality, either direction failing
the test by name) — this is what stops the table and the struct drifting apart, doing the job an
attribute macro would otherwise do by construction. The admin GET routes (Task 6) serialize
`SETTING_META` alongside the schema so the web tab renders the mark without hand-duplicating it
(Task 7 Step 1).

## Exact knob list (from the inventory, by struct field)

**`ClusterSettings.spec` (per-region agent tunables) — all `live`: the agent's beats already
re-read every one of these each cycle, so none of them needs a roll.**

| field | env var | default | range | mark | file:line today |
|---|---|---|---|---|---|
| `sync_secs` | `WS_SYNC_SECS` | 60 | 10..=3600 | live | `bins/agent/src/sync.rs:38` |
| `replica_secs` | `WS_REPLICA_SECS` | 300 | 30..=3600 | live | `bins/agent/src/peer/pull.rs:51` |
| `decommission_secs` | `WS_DECOMMISSION_SECS` | 30 | 5..=600 | live | `bins/agent/src/decommission.rs:27` |
| `node_dead_secs` | `WS_NODE_DEAD_SECS` | 180 | 60..=3600 | live | `bins/agent/src/peer/placement.rs:35` |
| `peer_send_timeout_secs` | `WS_PEER_SEND_TIMEOUT_SECS` | 3600 | 60..=21600 | live | `bins/agent/src/peer/pull.rs:77` |
| `peer_serve_timeout_secs` | `WS_PEER_SERVE_TIMEOUT_SECS` | 900 | 60..=21600 | live | `bins/agent/src/peer/mod.rs:280` |
| `peer_receive_slack` | `WS_PEER_RECEIVE_SLACK` | 3 | 0..=60 | live | `bins/agent/src/peer/pull.rs:39` |
| `stop_flush_timeout_secs` | *(new — currently uncapturable, see note)* | 30 | 5..=300 | live | n/a — spec names it; no current env var was found reading a stop-flush timeout in the inventory, so this field ships with the built-in default only until a caller reads it (see Task 3 Step 4) |
| `nix_timeout_secs` | `WS_NIX_TIMEOUT` | `DEFAULT_TIMEOUT_SECS` (1800, confirm at `bins/agent/src/nix.rs`'s const) | 60..=7200 | live | `bins/agent/src/nix.rs:72` |
| `nixpkgs` | `WS_NIXPKGS` | `""` | none (string pin, no numeric range — kept anyway per spec §2, which lists it explicitly as a cluster setting despite being unbounded; validated only for non-emptiness when set) | live | `bins/agent/src/nix.rs:68` |
| `base_packages` | `WS_BASE_PACKAGES` | `nix::DEFAULT_BASE_PACKAGES` | none (space-separated string) | live | `bins/agent/src/nix.rs:63` |
| `default_replicas` | *(new field — today `Volume.spec.replicas` default is wherever `/v1`'s create path hardcodes it)* | 2 | 1..=5 | live | `crates/workspaces/src/api/*` — grep at Task 2 Step 3 for the literal |
| `max_per_owner` | `WS_MAX_PER_OWNER` | 20 | 1..=1000 | live | `crates/workspaces/src/model.rs:34` — **api tier, not agent**, but the spec's table places it under cluster scope because it is per-region policy; see Task 2 note |
| `home_cache_gb` | *(new field — no current env var; the homecache subvolume has no quota today)* | 10 | 1..=500 | live | n/a |
| `quota_gb_ceiling` | *(the `clamp_quota` 500 constant)* | 500 | 10..=5000 | live | grep `clamp_quota` at Task 2 Step 3 (lands in `crates/workspaces`, quotas plan) |
| `default_image` | `WS_DEFAULT_IMAGE` | *(required today — no built-in default; ships with `""` = "keep the env value")* | none (image ref string) | **boot** — `rustic-git-agent` | `bins/agent/src/controller/mod.rs:232` |
| `git_init_image` | `WS_GIT_INIT_IMAGE` | `alpine/git:2.45.2` | none (image ref string) | **boot** — `rustic-git-agent` | `bins/agent/src/controller/mod.rs:263` |
| `runtime_class` | `WS_RUNTIME_CLASS` | `""` (empty = host kernel) | none (k8s runtimeClass name) | **boot** — `rustic-git-agent` | `bins/agent/src/controller/mod.rs:234` |

**`CentralSettings` (server/worker/gateway/api tunables), object-store document:**

| field | env var | default | range | mark | readers (if boot) |
|---|---|---|---|---|---|
| `max_body` | `RUSTIC_GIT_MAX_BODY` | 2147483648 | 1048576..=8589934592 | live | — |
| `max_layer` | `RUSTIC_GIT_MAX_LAYER` | 5368709120 | 1048576..=21474836480 | live | — |
| `max_manifest` | *(new — no current env var found for a manifest size cap; spec §2 names a third body limit)* | 4194304 | 65536..=67108864 | live | — |
| `upload_grace_secs` | `RUSTIC_GIT_UPLOAD_GRACE_SECS` | 86400 | 3600..=604800 | live | — |
| `gc_interval_secs` | *(new — GC's own interval was not found as an env var in the inventory; grep `crates/registry/src/gc.rs` for its scheduling at Task 4 Step 2)* | 3600 | 300..=86400 | live | — |
| `merge_lease_secs` | *(new — the merge-worker lease TTL; grep `crates/pulls/src/merge_worker.rs`)* | 300 | 30..=3600 | live | — |
| `announce_stranded_secs` | *(new — `announce_stranded_merges`'s 15s beat in `bins/server/src/lanes.rs`)* | 15 | 5..=300 | live | — |
| `feed_retention_secs` | *(new — `crates/storage/src/events.rs`'s feed retention, if any is enforced; else document as "not currently enforced, field is a no-op until a caller reads it")* | 604800 | 3600..=2592000 | live | — |
| `clone_host` | `RUSTIC_GIT_CLONE_HOST` | `""` (prod requires) | none | live | — (web reads it live via Task 4 Step 5's route, no restart needed) |
| `ssh_host` | `RUSTIC_GIT_SSH_HOST` | `""` | none | **boot** | `rustic-git-gateway` — the init container that clones over SSH reads this at pod start via the gateway's own connection info, not per-request |
| `ssh_port` | `RUSTIC_GIT_SSH_PORT` | 22 | 1..=65535 | **boot** | `rustic-git-gateway` |
| `registry_host` | `RUSTIC_GIT_REGISTRY_HOST` | `""` | none | live | — |
| `signup_open` | *(new — no such flag exists today; ships `true` and is a no-op until a caller reads it, OR is dropped from scope if brainstorming with the owner finds no signup gate exists — flagged as an ambiguity below)* | true | bool, no range | live | — |
| `log_format` | `RUSTIC_GIT_LOG_FORMAT` | `""` (text; `json` switches) | none (enum-shaped string, validated against `["", "json"]`) | **boot** | `rustic-git-srv`, `rustic-git-api`, `rustic-git-worker`, `rustic-git-gateway`, `rustic-git-admin` — `crates/core/src/log.rs:30` reads it once at `tracing_subscriber::init()`, before any settings handle exists |
| `worker_lanes` | `RUSTIC_GIT_WORKER_CONCURRENCY` | 4 | 1..=64 | **boot** | `rustic-git-worker` — `bins/worker/src/main.rs:76` spawns exactly `lanes` tasks at startup; changing the count needs a fresh process |

Fields marked "new" have no reader today: they are added to the struct and the admin UI per the
spec's instruction to un-cache "every non-secret, non-bootstrap tunable... including the 'cached
once' ones", but a field nobody reads is inert. Task 3/4 wire the ones with an obvious existing
call site (`stop_flush_timeout_secs` has none — no code currently enforces a stop's flush
deadline — so it is added to the struct/CRD/UI for completeness per the spec but left unread by
any beat; a `// ponytail: unread until a caller needs it` marks it in the struct). `default_image`,
`git_init_image`, `runtime_class`, `ssh_host`, `ssh_port`, `log_format` and `worker_lanes` are the
spec §2 "cached once"/boot rows folded into scope by Part 1 of this update — each already has a
call site (cited above), and each is `boot`-marked because un-caching it (reading it fresh per
pod/build/connection) would be a bigger, riskier diff than declaring it boot and rolling the
reader on change, exactly the tradeoff §2 describes.

Note on the Global Constraints' "Secrets and addresses are never settings" line: it names
`WS_DEFAULT_IMAGE`/`WS_RUNTIME_CLASS`/`WS_GIT_INIT_IMAGE` alongside process-identity env vars —
re-read against §2, which explicitly calls these three out as boot settings ("images for tenant
pods such as `WS_DEFAULT_IMAGE` and `WS_GIT_INIT_IMAGE`, `WS_RUNTIME_CLASS`"), that Global
Constraints line is corrected here: those three ARE settings (tenant pod images, not the agent's
own identity — CLAUDE.md's Workspaces section already draws this distinction: "Tenant pod images
... are settings because they are what the agent hands to tenants, not what the agent runs as").
The corrected Global Constraints bullet lives in the copy above; this note exists so a reader
diffing against the spec's verbatim Rules section understands why the two lists disagree.

## Task 1: `ClusterSettings` CRD, `crd_yaml` test, RBAC rows

- **Files:**
  - Modify: `crates/workspaces/src/crd/mod.rs` — new `ClusterSettings` kind
  - Modify: `crates/workspaces/tests/crd_yaml.rs` — regenerate the golden YAML
  - Modify: `deploy/k3s/agent-rbac.yaml` — agent `get/list/watch` on `clustersettings`
  - Modify: `deploy/k3s/api-rbac.yaml` — admin ServiceAccount `create/patch` on `clustersettings`
    (the quotas plan's `rustic-git-admin` ClusterRole; the user-role ServiceAccount gets nothing)
  - Modify: `deploy/k3s/crds.yaml` — the generated CRD manifest (via `CRD_REGEN=1 cargo test`)
- **Interfaces:** `crd::ClusterSettings`, `crd::ClusterSettingsSpec`, `crd::ClusterSettingsStatus`

Steps:

- [ ] **Step 1: Define the CRD.** In `crates/workspaces/src/crd/mod.rs`, beside the other
  `#[derive(CustomResource)]` kinds (`Volume`, `Workspace`, ...), add:

  ```rust
  /// One per region, named `default` — the cluster-scoped tunables every agent in that cluster
  /// reads on its refresh beat. `spec` is desired (admin-written); `status.observedGeneration`
  /// is the last generation an agent actually applied, so the UI's "pending" marker has
  /// something to compare against. Cluster-scoped like every other kind here: there is one
  /// object per region's k3s, not per namespace.
  #[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
  #[kube(
      group = "rustic-git.io",
      version = "v1alpha1",
      kind = "ClusterSettings",
      status = "ClusterSettingsStatus",
      selectable = "" // agents watch the single `default` object by name, not by field
  )]
  #[serde(rename_all = "camelCase")]
  pub struct ClusterSettingsSpec {
      /// Sync-point cut beat interval. 10..=3600 seconds.
      #[serde(default = "defaults::sync_secs")]
      pub sync_secs: u64,
      /// Replication pull beat interval. 30..=3600 seconds.
      #[serde(default = "defaults::replica_secs")]
      pub replica_secs: u64,
      /// Decommission-beat interval. 5..=600 seconds.
      #[serde(default = "defaults::decommission_secs")]
      pub decommission_secs: u64,
      /// How long a node must be observed NotReady before it is declared dead for placement.
      /// 60..=3600 seconds.
      #[serde(default = "defaults::node_dead_secs")]
      pub node_dead_secs: u64,
      /// `btrfs send`-over-HTTP client timeout. 60..=21600 seconds.
      #[serde(default = "defaults::peer_send_timeout_secs")]
      pub peer_send_timeout_secs: u64,
      /// The send side's own deadline, deliberately shorter than the client's. 60..=21600 seconds.
      #[serde(default = "defaults::peer_serve_timeout_secs")]
      pub peer_serve_timeout_secs: u64,
      /// Slack added to the receive-side timeout over the serve-side one. 0..=60 seconds.
      #[serde(default = "defaults::peer_receive_slack")]
      pub peer_receive_slack: u64,
      /// Deadline for a stop's flush before the pod is torn down anyway. 5..=300 seconds.
      // ponytail: no caller reads this yet; ships for the admin UI ahead of the enforcement it
      // is meant for. Add the read when a stop-flush deadline is actually implemented.
      #[serde(default = "defaults::stop_flush_timeout_secs")]
      pub stop_flush_timeout_secs: u64,
      /// Nix build timeout. 60..=7200 seconds.
      #[serde(default = "defaults::nix_timeout_secs")]
      pub nix_timeout_secs: u64,
      /// Nixpkgs revision pin (`github:NixOS/nixpkgs/<rev>`). Empty means "whatever the agent's
      /// own env default is" — this field does not carry a built-in default of its own.
      #[serde(default)]
      pub nixpkgs: String,
      /// Packages prepended to every workspace's profile, space-separated.
      #[serde(default = "defaults::base_packages")]
      pub base_packages: String,
      /// Default `Volume.spec.replicas` for a newly created volume. 1..=5.
      #[serde(default = "defaults::default_replicas")]
      pub default_replicas: u32,
      /// Max workspaces+environments per owner in this region, until `Quota` fully replaces it.
      /// 1..=1000.
      #[serde(default = "defaults::max_per_owner")]
      pub max_per_owner: u32,
      /// Home-cache local subvolume quota per (owner, node). 1..=500 GiB.
      #[serde(default = "defaults::home_cache_gb")]
      pub home_cache_gb: u32,
      /// Ceiling `clamp_quota` enforces on a requested quota. 10..=5000 GiB.
      #[serde(default = "defaults::quota_gb_ceiling")]
      pub quota_gb_ceiling: u32,
      /// Tenant workspace pod image. **Boot** — the agent reads this at pod-template render
      /// time, not per reconcile; a change rolls `rustic-git-agent` (Task 5). Empty means "keep
      /// today's env value" so an admin who never opens this row cannot blank a required image.
      #[serde(default)]
      pub default_image: String,
      /// The init container that clones a workspace's seed repo over SSH. **Boot**, same reason.
      #[serde(default = "defaults::git_init_image")]
      pub git_init_image: String,
      /// k8s `runtimeClassName` for tenant pods (e.g. `gvisor`); empty = host kernel. **Boot**.
      #[serde(default)]
      pub runtime_class: String,
  }

  #[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
  #[serde(rename_all = "camelCase")]
  pub struct ClusterSettingsStatus {
      /// The generation an agent last successfully applied. Compared against
      /// `metadata.generation` by the admin UI's pending marker.
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub observed_generation: Option<i64>,
  }
  ```

  Add a `defaults` submodule with one `fn` per default (`pub(crate) mod defaults { pub fn
  sync_secs() -> u64 { 60 } ... }`) — `serde(default = "path")` needs a path, and this keeps
  every default in one place `Settings::from_env` in `crates/workspaces/src/settings.rs` (Task 2)
  can also call, so the CRD default and the env-fallback default cannot drift apart.

  Beside `defaults`, add the mark table this update's "Which mechanism carries `mark`/`readers`"
  section decided on:

  ```rust
  pub const CLUSTER_SETTING_META: &[(&str, crate::settings_meta::Mark, &[&str])] = &[
      ("syncSecs", Mark::Live, &[]),
      ("replicaSecs", Mark::Live, &[]),
      ("decommissionSecs", Mark::Live, &[]),
      ("nodeDeadSecs", Mark::Live, &[]),
      ("peerSendTimeoutSecs", Mark::Live, &[]),
      ("peerServeTimeoutSecs", Mark::Live, &[]),
      ("peerReceiveSlack", Mark::Live, &[]),
      ("stopFlushTimeoutSecs", Mark::Live, &[]),
      ("nixTimeoutSecs", Mark::Live, &[]),
      ("nixpkgs", Mark::Live, &[]),
      ("basePackages", Mark::Live, &[]),
      ("defaultReplicas", Mark::Live, &[]),
      ("maxPerOwner", Mark::Live, &[]),
      ("homeCacheGb", Mark::Live, &[]),
      ("quotaGbCeiling", Mark::Live, &[]),
      ("defaultImage", Mark::Boot, &["rustic-git-agent"]),
      ("gitInitImage", Mark::Boot, &["rustic-git-agent"]),
      ("runtimeClass", Mark::Boot, &["rustic-git-agent"]),
  ];
  ```

  `Mark` itself lives in `crates/core/src/settings.rs` (Task 2 Step 1, since both tiers' meta
  tables need the same two-variant enum) and `crates/workspaces` depends on `crates/core` already
  (every crate here does), so `crate::settings_meta` above is really
  `rustic_git_core::settings::Mark` re-exported — name it however the existing `crates/workspaces`
  import conventions do (check a sibling `use rustic_git_core::...` line before inventing a path).
  A test, `cluster_setting_meta_is_exhaustive` (mirrors Task 2's central one), asserts the table's
  field names equal `ClusterSettingsSpec`'s schemars property names.

- [ ] **Step 2: `crd_yaml` test.** Run `CRD_REGEN=1 cargo test --test crd_yaml -p
  rustic-git-workspaces` to regenerate the golden file the test compares against, then `cargo
  test --test crd_yaml` to confirm it now passes without the env var. Diff the generated YAML by
  hand for the two load-bearing attributes `crd/mod.rs`'s doc comment calls out: `status = "…"`
  (status subresource) and NO `selectable` entry (this kind has no per-node watch — every agent
  in the region watches the single `default` object).

- [ ] **Step 3: RBAC rows.** In `deploy/k3s/agent-rbac.yaml`, add a rule to the agent's
  ClusterRole: `resources: ["clustersettings"]`, `verbs: ["get", "list", "watch"]` — no `patch`,
  the same pattern the header table documents for every other main resource the agent must not
  desired-state-edit. In `deploy/k3s/api-rbac.yaml` (from the quotas plan), add to the
  **admin** ClusterRole only: `resources: ["clustersettings"]`, `verbs: ["get", "create",
  "patch"]` (no `delete` — a `ClusterSettings` object is never removed, only its fields reset to
  defaults by omission). Confirm the quotas plan's user-role ClusterRole gets nothing on this
  resource — `crates/workspaces/src/api/mod.rs`'s `api::router` never touches `ClusterSettings`.

- [ ] **Step 4: Commit.**

  ```
  git commit -m "Add the ClusterSettings CRD and its RBAC rows"
  ```

## Task 2: `Settings` structs, `LiveSettings<T>` handle

- **Files:**
  - Create: `crates/workspaces/src/settings.rs` — `AgentSettings` (mirrors `ClusterSettingsSpec`
    field-for-field, since the agent works from a plain struct, not the CRD wrapper, so tests
    don't need a `kube::Client` to construct one), `AgentSettings::from_env()`,
    `AgentSettings::merge(stored: &ClusterSettingsSpec)`
  - Create: `crates/core/src/settings.rs` — `LiveSettings<T>(Arc<ArcSwap<T>>)`, generic over any
    `T: Send + Sync + 'static`; `CentralSettings` struct + `from_env()`/`merge()` lives here too
    (shared by server/worker/gateway/api, none of which depend on `crates/workspaces`)
  - Modify: `crates/core/Cargo.toml` — add `arc-swap = "1"` (already pinned in `Cargo.lock` via a
    transitive dependency; this promotes it to direct)
  - Modify: `Cargo.toml` (workspace) — nothing, `Cargo.lock` already resolves the version
- **Interfaces:** `rustic_git_core::settings::LiveSettings<T>::{new, load, store}`,
  `rustic_git_workspaces::settings::AgentSettings`, `rustic_git_core::settings::CentralSettings`

Steps:

- [ ] **Step 1: `LiveSettings<T>`.** In `crates/core/src/settings.rs`:

  ```rust
  //! One handle shape for every process that reads a settings document on a beat: load the
  //! current value with no lock contention on the hot path (`ArcSwap::load` is a single atomic
  //! read), swap in a new one from the refresh beat. Generic so the agent's `AgentSettings` and
  //! the central tier's `CentralSettings` share one type instead of two hand-rolled RwLocks.
  use arc_swap::ArcSwap;
  use std::sync::Arc;

  #[derive(Clone)]
  pub struct LiveSettings<T>(Arc<ArcSwap<T>>);

  impl<T> LiveSettings<T> {
      pub fn new(initial: T) -> Self {
          Self(Arc::new(ArcSwap::from_pointee(initial)))
      }
      /// The current value. Cheap enough to call at the top of every beat iteration — that is
      /// the whole point.
      pub fn load(&self) -> Arc<T> {
          self.0.load_full()
      }
      /// Refresh beats call this after a successful parse. Never called on a parse failure —
      /// "last good wins" is enforced by the CALLER simply not calling this, not by anything
      /// here.
      pub fn store(&self, new: T) {
          self.0.store(Arc::new(new));
      }
  }
  ```

- [ ] **Step 2: `CentralSettings`.** In the same file, the central-tier struct with every field
  from the table above, `#[derive(Serialize, Deserialize, JsonSchema)]`, `serde(default)` at the
  container level, plus:

  ```rust
  impl CentralSettings {
      pub fn from_env() -> Self { /* one field per env var, exactly the inventory's defaults */ }
      /// `stored ?? env ?? default`: `self` (already env ?? default from `from_env`) provides
      /// the floor, `stored`'s `Option<..>`-shaped twin overrides field by field. The stored
      /// document is `serde(default)` too, so a partial document (one field written, the rest
      /// never touched) merges cleanly.
      pub fn merged_with(mut self, stored: &StoredCentralSettings) -> Self { /* field assigns */ }
  }
  ```

  `StoredCentralSettings` is the wire type at `cluster/settings` — every field `Option<_>`
  (`serde(skip_serializing_if = "Option::is_none")`) plus `updated_by: String`, `updated_at:
  String` (RFC 3339), so a document that has never touched `maxBody` doesn't silently coerce it
  to `0`.

- [ ] **Step 3: `AgentSettings`.** In `crates/workspaces/src/settings.rs`, the mirror of
  `ClusterSettingsSpec` as a plain struct (not a `CustomResource` — the agent's beats should not
  need a `kube` type to unit-test against) with `from_env()` reading the same env vars
  `ClusterSettingsSpec`'s `defaults` module falls back to, and `merged_with(spec:
  &crd::ClusterSettingsSpec) -> Self` doing the same override. Grep
  `crates/workspaces/src/api/*.rs` for the literal `Volume.spec.replicas` default and the
  `clamp_quota` 500 constant (both "new" fields in the table) and replace their current hardcoded
  values with `settings.load().default_replicas` / `.quota_gb_ceiling` at the call sites — this
  is the one piece of Task 2 that touches non-settings code, because those two constants have no
  home to read from until this struct exists. `default_image`/`git_init_image`/`runtime_class`
  are mirrored too but are `Mark::Boot` — Task 3 reads them ONCE at `Ctx` construction (same
  moment `bins/agent/src/controller/mod.rs:232-263` reads their env vars today), never per
  reconcile; Task 5 is what makes a change to them take effect, by rolling the agent DaemonSet,
  not a live re-read.

- [ ] **Step 4: Unit tests.** `crates/core/src/settings.rs` (or a sibling `#[cfg(test)] mod
  tests`): `stored ?? env ?? default` precedence — set one env var, merge a stored document that
  overrides a different field, assert the third field is the built-in default. Range validation
  is Task 4/5's job (it lives in the admin write path, not the struct); this test only proves the
  merge order.

- [ ] **Step 5: Commit.**

  ```
  git commit -m "Add the Settings structs and the LiveSettings handle"
  ```

## Task 3: Agent — reflector, beats read the handle, `status.observedGeneration`

- **Files:**
  - Modify: `bins/agent/src/lib.rs` — spawn the `ClusterSettings` reflector, construct
    `LiveSettings<AgentSettings>`, thread it into `Ctx`
  - Modify: `crates/workspaces/src/controller/mod.rs` (or wherever `Ctx` lives in the review-agent
    worktree — confirm at `bins/agent/src/controller/mod.rs:141`) — add `settings:
    LiveSettings<AgentSettings>` field
  - Modify: `bins/agent/src/sync.rs` — `sync_interval()` reads `ctx.settings.load().sync_secs`
  - Modify: `bins/agent/src/decommission.rs` — `beat_interval()` reads
    `ctx.settings.load().decommission_secs`
  - Modify: `bins/agent/src/peer/pull.rs` — `replica_interval()` reads `.replica_secs`,
    `send_timeout()` (or its current name) reads `.peer_send_timeout_secs`, the receive-slack
    read at `pull.rs:39` reads `.peer_receive_slack`
  - Modify: `bins/agent/src/peer/mod.rs:280` — `serve_timeout()` (its current name at that line)
    reads `.peer_serve_timeout_secs`
  - Modify: `bins/agent/src/peer/placement.rs:35` — `node_dead_secs()` reads
    `ctx.settings.load().node_dead_secs` (needs `&Ctx` or the handle threaded in; check today's
    signature — it may be a free function called without `Ctx`, in which case it gains a
    `settings: &LiveSettings<AgentSettings>` parameter and every caller is updated)
  - Modify: `bins/agent/src/nix.rs` — `base_packages()`/`nixpkgs_pin()`/`build_timeout()` gain the
    same `&LiveSettings<AgentSettings>` parameter
  - Modify: `bins/agent/src/controller/workspace.rs` (or wherever `WS_MAX_PER_OWNER`/quota
    ceiling apply on the agent side, if anywhere — else this row only applies via `/v1` per Task
    2 Step 3)
- **Interfaces:** `Ctx::settings: LiveSettings<AgentSettings>`, each beat's interval/timeout
  function gains a `settings` parameter (or reads it off `Ctx` where it already has one)

Steps:

- [ ] **Step 1: Boot-time load + reflector.** In `bins/agent/src/lib.rs`, BEFORE `Ctx` is
  constructed (this is the one place order matters: `controller/mod.rs:232-263` reads
  `WS_DEFAULT_IMAGE`/`WS_GIT_INIT_IMAGE`/`WS_RUNTIME_CLASS` while building `Ctx`, and those are
  boot settings now — `stored ?? env ?? default` has to be resolved before that struct exists, not
  after): a synchronous one-shot GET of `ClusterSettings/default` (falling back to
  `AgentSettings::from_env()` alone if the object is missing or the GET fails — first boot, or a
  region that has never had one written), then `Ctx::new` takes the merged `AgentSettings` instead
  of reading `WS_DEFAULT_IMAGE` et al. itself. `AgentSettings::from_env()` seeds
  `LiveSettings::new(..)` with that same merged value; then a `kube::runtime::reflector` (or a plain `watcher::watcher` loop —
  match whatever pattern the review-agent worktree already uses for watching its own node's
  objects, since this is the first *cluster-wide singleton* watch the agent has done, not a
  per-node one) on `Api::<ClusterSettings>::all(client)` filtered to `metadata.name == "default"`.
  On every event where the object parses cleanly: `settings.store(AgentSettings::from_env()
  .merged_with(&obj.spec))`; on a parse failure (a future field, a hand-edit with a bad type):
  `tracing::warn!` once and skip the store — "last good wins" per the Global Constraints. A 30 s
  fallback re-GET (`SETTINGS_REFRESH_SECS`, itself bootstrap-only, default 30) covers a missed
  watch event; the watch is what makes a change land in seconds rather than up to 30 s late.

- [ ] **Step 2: Thread `settings` into `Ctx`.** Add the field, update every `Ctx::new`/test
  constructor in `bins/agent/src/testsupport.rs` to accept a `LiveSettings<AgentSettings>` (tests
  construct one directly with `LiveSettings::new(AgentSettings { sync_secs: 5, .. })` rather than
  going through env or a fake reflector — the whole point of the handle is that a beat's test
  never touches `std::env`).

- [ ] **Step 3: Rewrite each beat's interval read.** Every function in the Files list above loses
  its `std::env::var(...)` call and gains a parameter (or reads off `ctx` if it already has one)
  reading `settings.load().<field>`. Name each beat here as the plan promised:
  - `sync_beat` (`bins/agent/src/sync.rs`) — reads `sync_secs` at the top of its loop
  - `decommission_beat` (`bins/agent/src/decommission.rs`) — reads `decommission_secs`
  - `pull_beat`/the replication ticker (`bins/agent/src/peer/pull.rs`) — reads `replica_secs`
    each tick, and `peer_send_timeout_secs` fresh for each new send it starts (an in-flight send
    keeps its old deadline — nothing here cancels one)
  - the peer HTTP server's per-request serve deadline (`bins/agent/src/peer/mod.rs`) — reads
    `peer_serve_timeout_secs` per incoming request, not cached at listener-bind time
  - `retire_pass`/the dead-node sweep (`bins/agent/src/peer/placement.rs`,
    `node_dead_secs`) — reads it per sweep pass, exactly as it does today (this one was already
    "no (read per check, no cache)" in the inventory — Task 3's job here is only to switch the
    source from `std::env::var` to `settings.load()`, not to add caching where there was none)
  - nix builds (`bins/agent/src/nix.rs`) — `base_packages`/`nixpkgs`/`nix_timeout_secs` read per
    build, same as today, source switched

- [ ] **Step 4: `stop_flush_timeout_secs`.** No caller exists (see the knob-list note). Leave it
  unread; the `// ponytail:` marker on the CRD field from Task 1 Step 1 stands as the record.

- [ ] **Step 5: `status.observedGeneration`.** After a successful `settings.store(..)` from a
  watch/refresh event (not from the initial `from_env()`-only boot), patch
  `ClusterSettings/default`'s `/status` subresource: `observed_generation:
  Some(obj.metadata.generation)`. This is what the admin UI's pending marker compares against
  `metadata.generation` (Task 7).

- [ ] **Step 6: Recorder tests.** Using `crates/workspaces/src/kube_test.rs`'s mock client: seed
  a `ClusterSettings/default` with `syncSecs: 30`, run one reflector tick, assert
  `ctx.settings.load().sync_secs == 30`. A fake-clock test on `sync_beat` (the pattern
  `bins/agent/src/sync.rs`'s existing tests already use, if any — else a `tokio::time::pause()`
  test): start the beat with `sync_secs: 60`, advance the settings handle to `sync_secs: 10`
  mid-run, advance the fake clock, assert the NEXT tick fires at the new interval, not
  mid-sleep. A corrupt-CR test: patch `ClusterSettings/default` with a spec that fails to
  deserialize (wrong type on a field via a raw JSON patch bypassing the typed client), assert the
  handle's value is unchanged and one warn was logged.

- [ ] **Step 7: Commit.**

  ```
  git commit -m "Read the agent's beats from a live ClusterSettings handle"
  ```

## Task 4: Central — `cluster/settings` document, `PUT /api/admin/settings`, refresh beat, `/healthz` version

- **Files:**
  - Modify: `bins/server/src/router/route.rs` — nothing routing-wise: this key is a shared
    object-store document, not a per-repo database, so like `_catalog` and
    `/api/{owner}/images` it is servable on ANY node — no `BROWSE_TAILS` entry, no ownership key.
    Confirm this in a comment at the new handler rather than silently relying on it.
  - Create: `bins/server/src/router/admin_settings.rs` (or extend an existing peer-only admin
    module, matching whatever already holds `api_visibility`/`api_description`'s pattern) — `PUT
    /api/admin/settings`
  - Modify: `bins/server/src/main.rs` (and `bins/worker/src/main.rs`, `bins/gateway/src/main.rs`,
    `bins/api/src/main.rs`) — boot-time `CentralSettings::from_env()` load, spawn the refresh beat
  - Modify: `crates/core/src/metrics.rs` or wherever `/healthz` is served per binary — expose the
    loaded settings VERSION (a monotonic counter or the `updatedAt` string) alongside the
    existing `ok` body, since the spec says "central binaries expose the loaded version on their
    `/api/health`" but the codebase's actual health path is `/healthz` (`bins/gateway/src/tunnel.rs:142`,
    `deploy/rustic-git.yaml:169/470/475` — no `/api/health` route exists anywhere in the Rust
    tiers; that string in the spec is describing the same concept the code calls `/healthz`, not
    a route to invent from scratch — see Ambiguities below)
- **Interfaces:** `PUT /api/admin/settings` (peer listener, superadmin JWT + peer secret, body =
  `StoredCentralSettings` partial), `refresh_settings_beat` (spawned identically in all four
  central binaries)

Steps:

- [ ] **Step 1: The document.** `cluster/settings` at the object store, JSON-encoded
  `StoredCentralSettings` plus `history: Vec<StoredCentralSettings>` (newest first, truncated to
  10 — "the last ten versions are kept" per the spec) inline in the same document rather than ten
  separate keys, since it is one small object either way and one GET is cheaper than eleven.

- [ ] **Step 2: `PUT /api/admin/settings`.** Peer-listener-only (mounted the same way
  `browse_api/pulls.rs`'s write routes are — peer secret via `Trusted`/whatever middleware guards
  that router), and additionally requires `Claims.superadmin` in the caller's JWT (from the
  quotas plan) — the route trusts the peer secret to prove "this is the admin server calling",
  and the JWT to prove "this admin server itself checked the human is a superadmin"; belt and
  braces because the peer secret alone would let ANY peer-authenticated caller (the worker, other
  server nodes) write settings, which only the admin server should be able to trigger. Body:
  partial `StoredCentralSettings` (only changed fields — see `PUT`'s "Save writes only the
  changed fields" from the spec's §5). Handler:
  1. GET the current `cluster/settings` (or the built-in-default document if the key is missing).
  2. Merge the partial body onto it, field by field.
  3. Validate every changed field's range — return `422` naming the field and its range on the
     first violation, matching the shape `quota::refuse`'s 409 sentences use for consistency
     (`"{field} must be between {lo} and {hi}, got {value}"`).
  4. Push the OLD document onto `history` (cap 10, drop the oldest), stamp
     `updated_by`/`updated_at`, PUT the new document.
  5. `200` with the new document.
  A `revert` is the same handler called with the body set to `history[n]`'s full snapshot — no
  separate route; Task 7's admin UI constructs that body.

- [ ] **Step 3: Refresh beat, all four binaries.** One function, `rustic_git_core::settings::
  refresh_central_beat(store: Store, live: LiveSettings<CentralSettings>)`, spawned identically
  in `bins/server/src/main.rs`, `bins/worker/src/main.rs`, `bins/gateway/src/main.rs`,
  `bins/api/src/main.rs` right after their existing boot sequences: every
  `SETTINGS_REFRESH_SECS` (30, bootstrap-only), GET `cluster/settings`, on successful parse
  `live.store(CentralSettings::from_env().merged_with(&doc))`, on parse failure or a missing key
  (never written yet) leave the handle at its current value and warn once. Gateway does not open
  the object store today for anything else — confirm whether it already holds a `Store` handle;
  if not, this is the one place gateway gains an object-store dependency, and it should be the
  MINIMAL client (read-only GET on one key), not the full `open_store` used by server/worker/api.

- [ ] **Step 3b: Boot fields read once, before the beat.** `log_format` and `worker_lanes` are
  `Mark::Boot` (see the updated knob list) — each binary does the SAME synchronous one-shot GET
  of `cluster/settings` Task 3 Step 1 added for the agent, merges it with `from_env()`, and reads
  `.log_format`/`.worker_lanes` off that merged value exactly once: `crates/core/src/log.rs`'s
  `tracing_subscriber::init()` call (today reading `RUSTIC_GIT_LOG_FORMAT` directly) takes the
  resolved string instead, and `bins/worker/src/main.rs:76`'s `lanes` local takes the resolved
  count instead of `env("RUSTIC_GIT_WORKER_CONCURRENCY", "4")`. The refresh beat (Step 3) still
  stores the merged value into `LiveSettings<CentralSettings>` afterwards, same as every other
  field — a live handle exists for every field regardless of mark, since Task 7's admin UI reads
  the CURRENT value from it either way; only the *readers* differ (a beat vs. a fresh process).

- [ ] **Step 3c: Validate the two new boot fields' shapes.** `log_format` is not a numeric range —
  Step 2's validator gets a special-case: valid values are `""` and `"json"`, `422` naming the
  field and the allowed set on anything else. `worker_lanes` is an ordinary `1..=64` range check
  like every other integer field.

- [ ] **Step 4: Wire each "new" central field to a caller** where one plainly exists:
  `max_body` → `crates/core/src/httpx.rs`'s body-size check gains a `LiveSettings<CentralSettings>`
  parameter (replacing its `env::var` read); `max_layer` → `crates/registry/src/blobs.rs`'s
  `OnceLock` read is replaced (this is the one true "un-cache" in the central set — the inventory
  flagged it as `OnceLock`, first-read-wins); `upload_grace_secs` → `crates/registry/src/uploads.rs`'s
  GC-pass read; `gc_interval_secs`/`merge_lease_secs`/`announce_stranded_secs` wired at their
  respective beats if Task 4's grep (per the knob table) finds a live interval to replace, else
  left inert with the same `// ponytail:` marker Task 1 used. `clone_host`/`ssh_host`/`ssh_port`/
  `registry_host` are consumed by the WEB tier, not a Rust binary — Task 6 wires those; the central
  document still carries them because the web has no settings-refresh beat of its own and instead
  reads them from `/api/health`'s exposed version... no: re-checking the spec, web reads
  `RUSTIC_GIT_CLONE_HOST` etc. directly from ITS OWN env today (`web/apps/web/src/lib/clone.ts`),
  which is a separate Next.js process the central `cluster/settings` document cannot refresh live
  without web polling it — flagged as an ambiguity below; this plan ships those four fields as
  admin-editable in `cluster/settings` (so the value is visible/auditable in one place) and has
  web read them via a small server-side fetch to the api tier's `/v1/settings/central`-shaped
  read (added in Task 4 Step 5) rather than `process.env`, replacing `lib/clone.ts`'s reads.

- [ ] **Step 5: A read route for non-admin consumers.** `GET /v1/settings/central` (or reuse
  `api::router` if such a route naturally belongs there) returning only the display fields
  (`clone_host`, `ssh_host`, `ssh_port`, `registry_host`) — no auth beyond the existing `/v1`
  bearer, since these are already public-facing values shown in clone menus to any signed-in
  user. `lib/clone.ts`'s three `host()`-style functions call this instead of `process.env`.

- [ ] **Step 6: `/healthz` exposes the loaded version.** Extend each binary's `/healthz` handler
  (today a bare `"ok"` string per `crates/core/src/metrics.rs:41`/`bins/gateway/src/tunnel.rs:142`)
  to include the settings document's `updated_at` (or a monotonic version counter bumped on every
  `store()`) in the response body — JSON `{"ok": true, "settings_version": "..."}` — so an
  operator (or the admin UI's pending marker) can confirm a process has actually picked up a
  change without grepping logs.

- [ ] **Step 7: Recorder + unit tests.** The write path: 422 on an out-of-range field, a
  successful write pushes history and truncates at 10, a revert body round-trips. The refresh
  beat: a fake `Store` returning a corrupt document leaves `live.load()` unchanged (a
  `tokio::time::pause()` test analogous to Task 3 Step 6).

- [ ] **Step 8: Commit.**

  ```
  git commit -m "Serve central settings from a live object-store document"
  ```

## Task 5: Admin workload roll infrastructure — `admin::workloads::KNOWN`, roll-on-boot-change, manual roll, RBAC

Builds the machinery spec §7 requires BEFORE Task 6's settings-write handlers can call it: a
fixed workload list, a "patch the restart annotation" primitive, the ready/desired-blocking rule,
the manual roll route, and the AKS-side RBAC the admin server needs to do any of this. Task 6
then wires its two `PUT` handlers to call `roll_readers` after a successful write.

- **Files:**
  - Create: `crates/workspaces/src/api/workloads.rs` — `admin::workloads::KNOWN`,
    `list_workloads`, `roll_readers`, `WorkloadRef { scope: Scope, name: &'static str, kind: Kind
    }` (`Scope::Central` needs the AKS client; `Scope::Region(String)` needs that region's
    `kube::Client`, the same one Task 6 Step 2 already resolves for `ClusterSettings`)
  - Modify: `crates/workspaces/src/api/admin.rs` — mount `GET /admin/workloads`, `POST
    /admin/workloads/{scope}/{name}/roll`
  - Modify: `bins/api/src/main.rs` — construct the AKS in-cluster `kube::Client` the admin binary
    needs for Task 5/6's central-scope calls (see Step 1 — today `rustic-git-admin` only holds a
    k3s kubeconfig per region; it holds nothing for its OWN cluster)
  - Modify: `deploy/rustic-git.yaml` — `rustic-git-admin` ServiceAccount (new — today the
    Deployment sets `automountServiceAccountToken: false` and authenticates to k3s only via a
    mounted kubeconfig Secret; it has never needed to talk to its OWN cluster before), a `Role` +
    `RoleBinding` in the `rustic-git` namespace scoped to exactly `get/list/patch` on
    `deployments`/`statefulsets` named `rustic-git-srv`/`rustic-git-api`/`rustic-git-worker`/
    `rustic-git-admin` (NOT `rustic-git-web`, which lives in a namespace `rustic-git-web.yaml`
    controls — confirm its namespace at Task 5 Step 1 and either widen the Role's namespace or
    add a second Role there), flip `automountServiceAccountToken: true` on the admin Deployment
    only (server/api/worker/gateway keep it false — they have no business talking to their own
    cluster's API, only the admin process does)
  - Modify: `deploy/k3s/agent-rbac.yaml` — nothing (the agent is rolled BY the admin server's
    region kube client, which already has `patch` on `daemonsets` per Task 5 Step 1's grep — if
    not, add it there, scoped to `rustic-git-agent` in `kube-system`)
  - Create/modify a per-region gateway RBAC file if `deploy/k3s/gateway.yaml`'s namespace
    (`rustic-git-system`) has no admin-writable Role yet — grep before assuming one exists
- **Interfaces:** `admin::workloads::KNOWN: &[WorkloadRef]`, `roll_readers(readers: &[&str],
  reason: RollReason) -> Result<(), RollConflict>` (`RollReason::Setting(&str)` or
  `RollReason::Manual(String)`), `GET /admin/workloads`, `POST
  /admin/workloads/{scope}/{name}/roll`

Steps:

- [ ] **Step 1: Who's on AKS, who's per-region — confirm before coding.** `rustic-git-srv`
  (StatefulSet), `rustic-git-api`, `rustic-git-worker`, `rustic-git-admin` all live in namespace
  `rustic-git` on the AKS cluster (`deploy/rustic-git.yaml`, confirmed above). `rustic-git-web` is
  a separate Deployment in `deploy/rustic-git-web.yaml` — grep its `metadata.namespace`; if it is
  also `rustic-git` on AKS it needs the same Role's `resourceNames` list extended, if it is a
  DIFFERENT namespace or cluster it needs its own Role. **`rustic-git-gateway` is NOT a central
  AKS workload** — `deploy/k3s/gateway.yaml` deploys one per region, in that region's k3s, in
  namespace `rustic-git-system` (its own header comment: "One replica per pool node... the
  Cloudflare A records for `ws-<region>.khost.dev`"). The spec's §7 workload list (echoed
  verbatim into this plan's own task list) names `rustic-git-gateway` under "central" and then
  separately lists "the region gateway" under "per region" — this plan corrects that: there is
  only ONE gateway kind (`rustic-git-gateway`) and it is exclusively a per-region k3s Deployment;
  `admin::workloads::KNOWN`'s central half does NOT include a `rustic-git-gateway` entry, and its
  per-region half's "region gateway" row IS `rustic-git-gateway` in `rustic-git-system`, rolled
  through the same region `kube::Client` Task 6 already resolves for `ClusterSettings`. This also
  means the earlier "boot, readers: rustic-git-gateway" rows in this update's knob list
  (`ssh_host`, `ssh_port` under `CentralSettings`) name a PER-REGION reader from a CENTRAL
  document — `roll_readers` for those two fields must fan out to every region's gateway, not one
  AKS deployment; note this explicitly in Task 6 Step 2's roll call rather than assuming one
  `kube::Client` covers it.

  ```rust
  // crates/workspaces/src/api/workloads.rs
  pub enum Scope { Central, Region(String) }
  pub enum Kind { StatefulSet, Deployment, DaemonSet }
  pub struct WorkloadRef { pub scope: Scope, pub name: &'static str, pub kind: Kind }

  /// The fixed list `admin::workloads::KNOWN` names in this plan — never a free string. A roll
  /// target not in this list is a 404, both from the settings save path and the manual route.
  pub const KNOWN_CENTRAL: &[(&str, Kind)] = &[
      ("rustic-git-srv", Kind::StatefulSet),
      ("rustic-git-api", Kind::Deployment),
      ("rustic-git-worker", Kind::Deployment),
      ("rustic-git-web", Kind::Deployment),
      ("rustic-git-admin", Kind::Deployment),
  ];
  /// Per region: resolved against that region's `kube::Client`, same as `ClusterSettings`.
  pub const KNOWN_PER_REGION: &[(&str, Kind)] = &[
      ("rustic-git-agent", Kind::DaemonSet), // namespace kube-system
      ("rustic-git-gateway", Kind::Deployment), // namespace rustic-git-system
  ];
  ```

- [ ] **Step 2: The AKS in-cluster client.** `rustic-git-admin` runs ON AKS (it is one of the
  Deployments in `deploy/rustic-git.yaml`), so unlike its region calls — which go out over a
  mounted kubeconfig Secret to a DIFFERENT cluster — talking to its OWN cluster is the standard
  `kube::Client::try_default()` in-cluster config: a projected ServiceAccount token plus the
  cluster's own CA, both provided automatically once `automountServiceAccountToken: true` is set
  on the pod (Task 5's `deploy/rustic-git.yaml` change) — no kubeconfig, no Secret, nothing to
  rotate. `bins/api/src/main.rs`'s admin startup path constructs this client once at boot,
  alongside (not instead of) the existing per-region kubeconfig-based clients, and threads it into
  `ApiState` as `aks_client: kube::Client` (or whatever the quotas plan's existing state struct
  naming convention is — check before inventing a field name).

- [ ] **Step 3: `roll_readers`.** For each reader name in a boot field's `readers` list (or the
  manual route's single target): resolve its `WorkloadRef` from `KNOWN` (404 if absent), fetch its
  current `status.readyReplicas`/`status.desiredNumberScheduled` (StatefulSet/Deployment vs.
  DaemonSet field names differ — branch on `Kind`); if ready < desired (a previous roll still in
  flight), return `RollConflict { name, ready, desired }` and DO NOTHING — this is what makes the
  settings write path's "409, nothing written" promise (spec §7) atomic: Task 6 checks
  `roll_readers`' result BEFORE persisting the settings document/CR, not after. If clear: patch
  `spec.template.metadata.annotations["rustic-git.io/restarted-at"]` to `Utc::now().to_rfc3339()`
  via `Patch::Merge` (exactly what `kubectl rollout restart` sends — never a pod `delete`), and
  the audit annotations `rustic-git.io/rolled-by` (the caller's name), `/rolled-at` (same
  timestamp), `/roll-reason` (`RollReason::Setting(field)` → `"setting:{field}"`,
  `RollReason::Manual(reason)` → the free-text reason) on the SAME patch. Also append one line to
  the admin audit log (wherever the quotas plan's admin actions already log — reuse that
  sink, do not invent a second audit log).

- [ ] **Step 4: `GET /admin/workloads`.** Lists every `KNOWN` entry (central once, plus one row
  per region for the per-region half — regions come from `crd::Region`, same source Task 6 Step 2
  uses): `{ scope, name, kind, image, ready, desired, rollout_state, last_roll: { by, at, reason }
  }`. `image` and `rollout_state` (`"RollingOut"`/`"Stable"`) read straight off the fetched
  object; `last_roll` reads the three audit annotations if present, `null` if the workload has
  never been rolled through this mechanism. Task 8's read-only infra view (`GET /admin/infra`)
  calls this SAME lister for its central-workload rows rather than re-implementing the Deployment
  fetch — the two routes differ only in which extra fields they render (infra adds Ingress hosts
  and node decommission status; this route adds the roll audit trail), not in how they list
  workloads.

- [ ] **Step 5: `POST /admin/workloads/{scope}/{name}/roll`.** Body: `{ "reason": string }`,
  `reason` required (400 if empty — this is the one field validation the manual route adds beyond
  what the boot-triggered path needs, since a boot roll's reason is always `setting:<field>`).
  Calls `roll_readers(&[name], RollReason::Manual(body.reason))` for the one named target; `404`
  if `name` is not in `KNOWN` for that `scope`, `409` with `{ready, desired}` if it's still
  rolling, `200` with the patched object otherwise.

- [ ] **Step 6: RBAC.** `deploy/rustic-git.yaml`'s new `Role`/`RoleBinding` (Step 1's file list) —
  `get`/`list`/`patch` on `deployments.apps`/`statefulsets.apps`, `resourceNames` restricted to
  the four/five names in `KNOWN_CENTRAL`, namespace `rustic-git` (extended to `rustic-git-web`'s
  namespace per Step 1's grep). Per region: `deploy/k3s/agent-rbac.yaml`'s existing table already
  documents every verb the AGENT's own ClusterRole holds — this is a DIFFERENT ClusterRole
  (`rustic-git-admin`'s, defined in `deploy/k3s/api-rbac.yaml` per the quotas plan), which needs a
  new row: `get/list/patch` on `daemonsets` (`apps`, `resourceNames: ["rustic-git-agent"]`,
  namespace `kube-system`) and on `deployments` (`resourceNames: ["rustic-git-gateway"]`,
  namespace `rustic-git-system`). No `delete`, no `create` — a roll only ever patches an existing
  object.

- [ ] **Step 7: Recorder tests.** `kube_test.rs`'s mock client: `roll_readers` on a workload with
  `ready == desired` patches the annotation and returns Ok; on `ready < desired` returns
  `RollConflict` and the mock records zero patch calls (proves "nothing written" is enforced here,
  not just documented); `GET /admin/workloads` shape against seeded Deployments/StatefulSet/
  DaemonSet objects; the manual route's 400 (empty reason), 404 (unknown name), 409 (mid-rollout).

- [ ] **Step 8: Commit.**

  ```
  git commit -m "Add the admin workload roll mechanism and RBAC"
  ```

## Task 6: Admin server routes — `GET/PUT /admin/settings/central`, `/admin/settings/clusters/{region}`

- **Files:**
  - Modify: `crates/workspaces/src/api/admin.rs` (from the quotas plan — `api::admin::router()`)
    — add the two settings routes
  - Modify: `bins/api/src/main.rs` — the admin router needs the object-store `Store` handle for
    the central route (today `bins/api` already opens one for the shared `open_store` bootstrap
    per the inventory's api-tier table) and a per-region `kube::Client` for the cluster route
    (already held for every other region-scoped admin route in the quotas plan)
- **Interfaces:** `GET /admin/settings/central`, `PUT /admin/settings/central` (proxies to the
  server tier's peer route from Task 4 Step 2 — the admin server has no direct object-store write
  path of its own by design, matching the spec's "written by the admin server through a new
  peer-only route on the server tier"), `GET /admin/settings/clusters/{region}`, `PUT
  /admin/settings/clusters/{region}` (writes `ClusterSettings/default` in that region's cluster
  directly via `kube`, same shape as every other admin cluster-scoped write)

Steps:

- [ ] **Step 1: `GET/PUT /admin/settings/central`.** `GET` proxies a read of `cluster/settings`
  (the admin server can read the object store directly — no need to round-trip through the
  server tier for a read, matching how `_catalog`/`/api/{owner}/images` are servable anywhere).
  `PUT`, before forwarding anything:
  1. Diff the incoming partial body against the current document to find which CHANGED fields are
     `Mark::Boot` (`CENTRAL_SETTING_META`, Task 2). `ssh_host`/`ssh_port` changed → readers are
     "every region's `rustic-git-gateway`" (Task 5 Step 1's note), resolved from `crd::Region`;
     `log_format`/`worker_lanes` changed → readers are the fixed central list from `KNOWN_CENTRAL`.
     No boot field changed → skip to step 3.
  2. Call `roll_readers`' pre-check (ready == desired for every affected reader) with NO patch
     yet. Any reader still mid-rollout → `409` with `{name, ready, desired}` for the first one
     found, and STOP — nothing is forwarded to the server tier, so `cluster/settings` is
     untouched (spec §7's "the settings write is NOT made").
  3. Forward the validated body to the server tier's peer route (`PUT /api/admin/settings`, Task
     4 Step 2) with the peer secret and re-mints/forwards the caller's superadmin JWT — this is
     the ONE place `api::admin` calls out to the git-tier peer listener, matching the pattern
     `bins/worker`'s merge jobs already use for peer calls (`local()` vs `networked()` split —
     never format the peer secret into an error message here either).
  4. On a successful write, call `roll_readers` for real (Task 5 Step 3) — patches the restart
     annotation with `RollReason::Setting(field)` per changed boot field, audits it. A roll
     starting between steps 2 and 3 is a real but narrow race (this route holds no lock across the
     network round-trip to the server tier) — `// ponytail: step-2/step-4 TOCTOU on a reader
     starting its own manual roll mid-save; a global "settings write" mutex in the admin process
     closes it if it's ever hit in practice, not built ahead of evidence it happens.`

- [ ] **Step 2: `GET/PUT /admin/settings/clusters/{region}`.** `GET` reads
  `ClusterSettings/default` from that region's `kube::Client` (resolved the same way every other
  per-region admin route resolves one — via `Region`, per the quotas plan's `crd::Region`).
  `PUT` validates ranges (same 422 shape as Task 4 Step 2 — factor the range-check function into
  `crates/core/src/settings.rs` so both write paths call the identical validator instead of two
  copies that could drift); diffs the incoming patch against the current spec using
  `CLUSTER_SETTING_META` (Task 1) for which changed fields are `Mark::Boot`
  (`default_image`/`git_init_image`/`runtime_class` → reader `rustic-git-agent` in THIS region);
  pre-checks `roll_readers` for that region's `kube::Client` the same way Step 1 does — a reader
  mid-rollout is `409` with `{ready, desired}` and the CR is not touched; only then stamps
  `rustic-git.io/updated-by`/`updated-at` annotations (per the spec's §4), server-side-applies the
  merged spec with the admin's field manager, and on success calls `roll_readers` for real
  (`RollReason::Setting(field)`). Returns the new spec plus `status.observedGeneration` (likely
  stale immediately after the write — that staleness IS the pending marker Task 3 Step 5/Task 7
  render).

  ```rust
  // crates/workspaces/src/api/admin.rs
  async fn put_cluster_settings(
      State(s): State<Arc<ApiState>>,
      Path(region): Path<String>,
      Json(body): Json<ClusterSettingsPatch>,
  ) -> Result<Json<crd::ClusterSettings>, Response> {
      body.validate()?; // shared with Task 4's central validator's range table
      let client = client_for_region(&s, &region).await?;
      let api: Api<crd::ClusterSettings> = Api::all(client);
      let current = api.get("default").await.unwrap_or_default();
      let boot_readers = workloads::boot_readers_changed(&current.spec, &body, crate::settings::CLUSTER_SETTING_META);
      if !boot_readers.is_empty() {
          workloads::precheck_readers(&client, &boot_readers).await?; // 409 on any still rolling, nothing written below
      }
      let merged = current.spec.merged_with_patch(body, &caller.name);
      let patched = api.patch(
          "default",
          &PatchParams::apply(crd::AGENT_FIELD_MANAGER_ADMIN /* new const, distinct from the agent's field manager */),
          &Patch::Apply(&merged),
      ).await.map_err(kube_err)?;
      for field in &boot_readers {
          workloads::roll_readers(&client, &[field.reader], RollReason::Setting(field.name)).await.ok();
          // .ok(): the precheck already refused a conflicting roll; a failure here is a transient
          // API error on an already-committed settings write, logged and surfaced on the next
          // `GET /admin/workloads` poll rather than rolled back — the settings document is the
          // source of truth per the Global Constraints, and it is already correct.
      }
      Ok(Json(patched))
  }
  ```

- [ ] **Step 3: Ten-version history + revert for the cluster scope.** An annotation,
  `rustic-git.io/settings-history`, carrying the previous spec as JSON, list-capped at 10 —
  parallel to the central document's inline `history` array. `PUT
  /admin/settings/clusters/{region}/revert/{n}` (or a `revert: true` field on the same PUT body
  naming an index) writes that historical spec back as the new current one, itself pushed onto
  history in turn (a revert is a write, not a rewind — the spec's own history keeps growing).

- [ ] **Step 4: Recorder tests.** Central: 422 propagates through the proxy, a successful PUT's
  history round-trips through the server tier's peer route (mock `kube_test`-style HTTP, not a
  real server process). Cluster: the same shape against `kube_test.rs`'s mock client — annotation
  written, 422 on an out-of-range field, revert restores the named history entry and appends a
  new history entry for the restore itself.

- [ ] **Step 5: Commit.**

  ```
  git commit -m "Add the admin settings routes for both scopes"
  ```

## Task 7: Web `/admin/settings`

- **Files:**
  - Create: `web/apps/web/src/app/admin/settings/page.tsx` — the two-tab shell
  - Create: `web/apps/web/src/app/admin/settings/central-tab.tsx`,
    `.../clusters-tab.tsx` — one row-table component each, reused for both (a shared
    `SettingsTable` component parameterized by the field list + save handler is the lazier shape —
    write ONE table component, not two, since the row shape — name/description/default/env/
    stored/range/last-change/pending — is identical across scopes)
  - Modify: `web/apps/web/src/lib/api.ts` (or wherever the admin API base lives per the quotas
    plan's `RUSTIC_GIT_ADMIN_API_URL`/`adminCall`) — add `getCentralSettings`,
    `putCentralSettings`, `getClusterSettings(region)`, `putClusterSettings(region, patch)`
  - Modify: `web/apps/web/src/lib/clone.ts` — the three `host()` functions call `GET
  /v1/settings/central` (Task 4 Step 5) instead of `process.env.RUSTIC_GIT_CLONE_HOST` etc.
- **Interfaces:** `SettingsTable` (props: `rows: SettingRow[]`, `onSave: (changed: Partial<T>) =>
  Promise<void>`), `SettingRow = { key, description, unit, value, envValue, defaultValue, range,
  mark: "live" | "boot", readers: string[], lastChangedBy, lastChangedAt, pending: boolean }`

Steps:

- [ ] **Step 1: `SettingsTable`.** One row per field: name, description (from the Rust struct's
  doc comment — since schemars exports `#[doc]` text into the JSON schema's `description`, the
  admin GET routes (Task 6) should return the schema AND `*_SETTING_META` (Task 1/2) alongside the
  values so the web tab never hand-duplicates field descriptions or the mark table), current
  value (an editable input, type inferred from the schema: number for a range field, text for a
  string, checkbox for a bool), env value and built-in default (both read-only, greyed), range
  (shown as help text under the input, and as client-side min/max on the number input so the 422
  is rarely hit), last change (who/when, from the row's own metadata), a mark badge ("live" or
  "boot: {readers}"), and a "pending" dot that shows while the row's value differs from what the
  last-known `observedGeneration`/health-version poll reports as applied (poll every few seconds
  after a save, same idea as any optimistic-UI spinner, clear it once the polled value matches).

- [ ] **Step 2: Save confirmation.** Before the diff is posted, if any changed row's `mark` is
  `"boot"`: a confirmation dialog listing exactly what Task 5/6's write path is about to do —
  "Save and roll: {reader list, deduped across every changed boot field}" (spec §7's own wording).
  If `rustic-git-srv` is among the readers (only reachable from a central `PUT`, since no cluster
  boot field's reader is ever the StatefulSet), a SECOND confirmation naming the DB-ownership-move
  risk in one sentence, pointing at CLAUDE.md's "Deploying" section language ("moves database
  ownership between nodes") rather than re-explaining the mechanism. Only live-marked changes:
  save with no dialog. A `409` from the save (a reader still rolling) surfaces the `{ready,
  desired}` body as a plain error — "rustic-git-worker is still rolling out (2/3 ready); try
  again shortly" — no retry loop, the operator retries by hand.

- [ ] **Step 3: Central tab.** Fetches `GET /admin/settings/central`, renders one `SettingsTable`,
  Save posts only the rows the user touched (a plain object diff against the fetched snapshot)
  through Step 2's confirmation flow to `PUT /admin/settings/central`.

- [ ] **Step 4: Clusters tab.** One region selector (reusing whatever region-list component the
  quotas plan's admin area already has for its own per-region panels) plus one `SettingsTable` per
  selected region, same fetch/diff/confirm/save shape against `/admin/settings/clusters/{region}`
  (no second-confirmation case here — no cluster-scope boot field's reader is `rustic-git-srv`).

- [ ] **Step 5: Roll progress.** After a boot save, poll `GET /admin/workloads` (Task 5 Step 4,
  filtered client-side to the readers just rolled) every few seconds and render each as
  `ready/desired` under the saved row until every one reaches `ready == desired` — this is the
  "roll progress from rollout status" the task asked for, and it is the SAME poll `SettingRow`'s
  `pending` dot already needs (Step 1), so one polling hook drives both rather than two.

- [ ] **Step 6: `lib/clone.ts` migration.** Replace the three `process.env.RUSTIC_GIT_*` reads
  with a server-side fetch to `/v1/settings/central` (Next.js server components can call this at
  render time; a short in-memory cache, a plain module-level `let cached: {value, at}`, TTL a few
  seconds, avoids hammering the api tier on every page render — NOT `LiveSettings`, that type is
  Rust-only; the lazy version here is a closure with a timestamp check, not a new dependency).

- [ ] **Step 7: `bun test`.** `SettingsTable`'s diff-computation (only changed rows are sent), the
  confirmation dialog's boot-vs-live branch and the second-confirmation-on-`rustic-git-srv` case,
  the pending/roll-progress poll-and-clear logic with a fake timer.

- [ ] **Step 8: Commit.**

  ```
  git commit -m "Add the admin settings UI"
  ```

## Task 8: Read-only infrastructure view

- **Files:**
  - Modify: `crates/workspaces/src/api/admin.rs` — `GET /admin/infra` (or fold into an existing
    admin overview route if the quotas plan already built one — check before adding a new route)
  - Modify: `web/apps/web/src/app/admin/settings/page.tsx` (or a sibling `/admin/infra` page,
    matching whichever the spec's §6 implies — it says "the same area", so a third tab beside
    Central/Clusters reads more naturally than a separate URL)
- **Interfaces:** `GET /admin/infra` → `{ tier: string, image: string, replicas: u32 }[]` plus
  ingress hosts and each node's decommission status (`rustic-git.io/decommission-status`
  annotation, per CLAUDE.md's workspaces section) — read via the k3s API (Deployments, Nodes),
  NOT stored anywhere new.

Steps:

- [ ] **Step 1: The route.** Calls Task 5's `admin::workloads::list_workloads` for the
  image/replica-count rows (reusing the same lister `GET /admin/workloads` uses rather than a
  second Deployment fetch), adds Ingress hosts from the two Ingress objects CLAUDE.md's Deploying
  section calls out (registry vs app hostname), and Nodes for the `decommission-status`
  annotation. Entirely read-only — no write handler exists for this route, by design (spec §6:
  "No writes").

- [ ] **Step 2: Web tab.** A third tab, plain read-only table, no inputs, refetched on tab
  focus (no live-update mechanism needed — an operator checking image pins is not on a beat).

- [ ] **Step 3: Recorder test.** The route against `kube_test.rs`'s mock: seeded Deployments/
  Nodes/Ingresses in, the expected shape out.

- [ ] **Step 4: Commit.**

  ```
  git commit -m "Add the read-only infrastructure view"
  ```

## Task 9: Docs + e2e

- **Files:**
  - Modify: `CLAUDE.md` — a short new subsection (placement: right after "Workspaces and
    environments" or as its own short section before "Web app" — it touches both the agent tier
    and the central tier, so it does not belong nested under either) documenting the
    `stored ?? env ?? default` rule, the `LiveSettings<T>` shape, the "next beat, never
    mid-operation" guarantee, and the live/boot split with its roll mechanism, at the density of
    the existing sections (WHY, not a field list — the field list lives in code/schemars, not
    prose)
  - Modify: `tests/ws_e2e.sh` — a new assertion block
- **Interfaces:** none new; this task only proves what Tasks 1–8 built.

Steps:

- [ ] **Step 1: CLAUDE.md.** Three or four sentences: where the truth lives (object-store document
  / `ClusterSettings` CR), the merge order, the one thing every future contributor must not do
  (read `std::env::var` directly for a knob that has a `Settings` field — go through
  `LiveSettings` instead, matching how the CLAUDE.md already tells contributors to route through
  `App::election_tick`/`OwnershipStore` rather than reinvent leader election), and the live/boot
  split — a boot field's reader is one of `admin::workloads::KNOWN`, a save rolls it by patching
  `rustic-git.io/restarted-at` (never a pod delete), and a reader still mid-rollout blocks the
  save with a 409 rather than letting the document run ahead of the pods.

- [ ] **Step 2: e2e.** In `tests/ws_e2e.sh` (already asserting a real agent+k3s round trip per
  CLAUDE.md): after the existing workspace push/pull assertions, `kubectl patch clustersettings
  default --type merge -p '{"spec":{"syncSecs":10}}'`, wait a bit over 10s (not the old 60s
  default), assert a NEW sync-point `Snapshot` was cut for a running worktree in that window —
  the live proof the spec's Testing section asks for ("change `syncSecs` on the k3s cluster from
  the UI and watch the cut cadence change within a minute"; the script drives it via `kubectl`
  rather than the UI, since `ws_e2e.sh` has no browser driver — note this substitution in the
  script's own comment). Exit 77 on the same missing-prerequisite conditions the rest of the
  script already uses.

- [ ] **Step 3: e2e for a boot roll.** `kubectl patch clustersettings default --type merge -p
  '{"spec":{"gitInitImage":"alpine/git:2.45.3"}}'` (a harmless tag bump on an already-pinned
  image, not a functional change — the point is proving the roll, not the new image), then
  `kubectl rollout status daemonset/rustic-git-agent -n kube-system` and assert it completes and
  the pod template's `rustic-git.io/restarted-at` annotation is fresh; assert the same patch sent
  TWICE in quick succession (before the first roll finishes) yields the documented 409 on the
  second — the concrete "nothing written" proof for the boot path, distinct from Step 2's live
  path. Exit 77 on the same missing-prerequisite conditions.

- [ ] **Step 4: Commit.**

  ```
  git commit -m "Document live settings and assert the sync-interval change end to end"
  ```

## Spec coverage

§1 Two scopes → Task 1 (`ClusterSettings`), Task 4 (`cluster/settings`). §2 Which knobs → the
knob-list table above, sourced from the inventory's candidates plus every cached-once field the
spec names — including the boot rows this update added (`default_image`, `git_init_image`,
`runtime_class`, `ssh_host`, `ssh_port`, `log_format`, `worker_lanes`) — each wired to a real call
site in Task 3/4 or marked `ponytail:` inert where none exists. §3 How a process reads them →
Task 2 (`LiveSettings<T>` + the `mark`/`readers` const tables), Task 3 (agent reflector, per-beat
reads for live fields, one-shot boot-time read for boot fields), Task 4 Step 3/3b (central refresh
beat, and the boot-field one-shot read before it). §4 Validation, audit, safety → Task 1 RBAC,
Task 4 Step 2/3c / Task 6 Steps 1–3 (range 422s, `log_format`'s enum check, annotations,
ten-version history, revert, "last good wins" in Task 3 Step 1 / Task 4 Step 3). §5 The admin UI →
Task 7. §6 Read-only infrastructure view → Task 8 (reusing Task 5's workload lister). **§7 A boot
setting change rolls its readers → Task 5 (the roll mechanism, `KNOWN`, RBAC, manual route) and
Task 6 Steps 1–2 (the settings write paths' precheck-then-write-then-roll sequencing); the UI side
(confirmation, second confirmation on `rustic-git-srv`, roll progress) → Task 7 Steps 2/5.** Rules
→ the Global Constraints block, enforced concretely by: env-then-stored-then-store order (Task 2
Step 2/3's `merged_with`), next-beat-never-mid-operation (Task 3 Step 3's per-beat reads, never a
running operation's parameter), last-good-wins (Task 3 Step 1, Task 4 Step 3), range validation
(Task 4 Step 2, Task 6 Step 2), secrets/process-identity excluded (the corrected Global Constraints
list, checked against every field the knob-list table admits), boot-rolls-readers/live-rolls-
nothing (Task 5 Step 3's `roll_readers`, called only for changed `Mark::Boot` fields in Task 6
Steps 1–2), roll-is-annotation-never-delete-or-free-name (Task 5 Step 3's `Patch::Merge` on the
restart annotation, `KNOWN`-only targets in Step 5). Cases table → covered by the recorder tests
in Tasks 3/4/5/6 and the live proof in Task 9. Testing → Task 2 Step 4 (precedence unit test),
Task 3 Step 6 (agent recorder + fake-clock), Task 4 Step 7 / Task 5 Step 7 / Task 6 Step 4 (admin
write-path and roll-mechanism recorder), Task 9 Step 2 (live).

## Ambiguities resolved while planning

- The spec's §7 workload list (and this update's own task instructions, which echo it) names
  `rustic-git-gateway` under "central" and separately lists "the region gateway" per region — read
  against `deploy/k3s/gateway.yaml` (one Deployment per region, namespace `rustic-git-system`,
  hostPort 80 per pool node) and `deploy/rustic-git.yaml` (which has no gateway Deployment at
  all), there is only ONE `rustic-git-gateway` kind and it is exclusively per-region. Task 5 Step 1
  resolves this: `admin::workloads::KNOWN_CENTRAL` has no gateway row; the per-region row IS
  `rustic-git-gateway`, and it is what `ssh_host`/`ssh_port`'s boot-reader entries in the central
  knob table actually name (a central document with a per-region reader — Task 6 Step 1 fans the
  roll out to every region rather than assuming one client covers it).
- The spec's §5 says central binaries expose "the loaded version on their `/api/health`", but no
  `/api/health` route exists anywhere in the Rust tiers — the actual route, everywhere, is
  `/healthz` (`crates/core/src/metrics.rs`, `bins/gateway/src/tunnel.rs:142`,
  `deploy/rustic-git.yaml`). Read this as describing the health-check concept, not a literal new
  path; Task 4 Step 6 extends the existing `/healthz` body instead of adding a second route.
- `WS_MAX_PER_OWNER` is read by the API tier (`crates/workspaces/src/model.rs:34`), not the
  agent, but the spec's cluster-scope table lists it under `ClusterSettings` — kept as written
  (per-region policy fits the CR's cluster-scoped shape better than a central document that would
  otherwise need a per-region dimension bolted on), with a note in the knob table that its reader
  lives on the api tier and reads the same CR through its own region's `kube::Client`, not
  through `LiveSettings<AgentSettings>`.
- Four fields have no reader anywhere in the current codebase (`stop_flush_timeout_secs`,
  `max_manifest`, `gc_interval_secs`'s literal interval if none is found at Task 4 Step 4's grep,
  `signup_open`) — these ship as struct fields and admin-UI rows per the spec's explicit
  instruction to un-cache "every... tunable, including the 'cached once' ones," but are marked
  `// ponytail: unread until a caller needs it` rather than wired to invented behavior; wiring
  them is a follow-up once the feature they'd gate actually exists.
- `RUSTIC_GIT_CLONE_HOST`/`SSH_HOST`/`SSH_PORT`/`REGISTRY_HOST` are consumed by the Next.js web
  process, which has no beat and no `LiveSettings` handle of its own (that type is Rust-only) —
  resolved by having web read them through a new `GET /v1/settings/central` route (Task 4 Step 5)
  at render time with a short in-memory cache, rather than inventing a JS port of `ArcSwap`.
