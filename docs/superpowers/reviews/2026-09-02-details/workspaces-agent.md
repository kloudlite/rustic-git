# Review — `crates/workspaces`, `bins/api`, `bins/agent`

Read in full: `crates/workspaces/src/{api,k8s,crd,model,packages,replicate}.rs`,
`crates/workspaces/src/engine/{mod,pool,ops,commit}.rs`,
`bins/agent/src/{controller,peer,snapshot,sync,claim,janitor,binding,lib}.rs`,
`deploy/k3s/workspace-admission.yaml`. Test inventories from
`bins/agent/tests/{reconcile,peer}.rs` and the in-crate `mod tests`.

General note: this is a well-defended codebase. The keep-bias rule is applied consistently
(every list error in `pull_volume`, `retain`, `retire_pass`, `janitor_sweep_*`, `unclaim_kind`
aborts rather than deletes), the claim/unclaim/takeover writes all use guarded
`replace_status` / JSON-patch `test`, and the two-level validation rule
(`validate_mount`, `validate_service`, `git_init_container`, `packages::validate_list` all
re-check at the agent) is honoured almost everywhere. The findings below are the exceptions.

**Counts: Critical 0 · High 2 · Medium 6 · Low 4**

---

## Correctness and security

### 1. High — `workspace_pod` is the one builder that does not re-validate its untrusted input
`crates/workspaces/src/k8s.rs:847` (`workspace_pod`), consuming `spec.name` at
`k8s.rs:868` (`prelude`), `k8s.rs:905` (`workspace_dir` as `mount_path`), `k8s.rs:429`→`k8s.rs:268`
(`sshd_config`'s `SetEnv`).

`spec.name` is interpolated verbatim into a `/bin/sh -c` script that runs as root in the pod
(`prelude`, k8s.rs:384-407 — it contains `chown`, `printf` and `su` with the name spliced in), into
quoted `SetEnv` values in the generated `sshd_config`, and into the container's `mount_path`.
Its two sibling builders both re-validate for exactly this reason and say so:
`git_init_container` re-checks `repo`/`branch` (`k8s.rs:792-802`, "this is the last place before
the value becomes an ssh argv … it also covers a Volume written by another path") and
`service_statefulset` re-runs `validate_service` (`k8s.rs:1020`). `workspace_pod` does not, and
`apply_workspace` (`controller.rs:2271`) never checks `model::valid_ws_name` either — the only
check is `check_ws_name` in `/v1` (`api.rs:507`). By CLAUDE.md's own rule ("any principal with
write access to the object — a restored backup, a migration, an operator with kubectl — can put an
arbitrary list there"), a `Workspace` written by any other path yields root shell injection inside
that workspace's own pod and an arbitrary container-side mount target
(`name = "../../etc"` → `mount_path: /home/kl/workspaces/../../etc`).

Blast radius is the tenant's own pod, not the node (the pod fence in
`deploy/k3s/workspace-admission.yaml` still bounds `hostPath` to `/nix` and `/wspool-prod/`), which
is why this is High and not Critical.

**Fix:** make `workspace_pod` fallible like `service_statefulset`, opening with
`if !model::valid_ws_name(&spec.name) { return Err(format!("workspace name {:?} is not a name", spec.name)) }`,
and settle it `Outcome::Permanent(.., "InvalidName")` at the call site — the same shape
`git_init_container`'s `Err` already gets at `controller.rs:2248-2267`.

### 2. Medium — `spec.owner` becomes a root-run path with no `valid_owner` check
`bins/agent/src/controller.rs:1910` (`ensure_shared_home` → `homes_root(pool).join(owner)` +
`std::os::unix::fs::chown` as root, `controller.rs:1918-1924`),
`crates/workspaces/src/engine/ops.rs` (`ensure_homecache` → `{pool}/homecache/{owner}`,
`btrfs subvolume create`), `crates/workspaces/src/k8s.rs:692`/`700` (`home_volume`/`homecache_volume`
format it into a `hostPath`).

Nothing on the agent side validates `spec.owner` before it is joined onto the pool root and
chowned by a privileged process. It is blocked today only by accident: `apply_workspace`'s first
statement is `heal_labels` (`controller.rs:1929` → `controller.rs:586`), which patches the same
string as a *label value*, and the API server rejects a label containing `/` or `..` — so the
reconcile errors out before reaching `ensure_shared_home`. Reorder or drop that call (a plausible
refactor: `heal_labels` is cosmetic) and the traversal is live. The pod fence catches the
`hostPath` half but not the agent's own `mkdir`/`chown`.

**Fix:** one guard at the top of `apply_workspace`/`apply_environment` —
`kloudlite_git_storage::store::valid_owner(&spec.owner)`, settled `Permanent`. A `pattern` on the
CRD schema for `owner` would be the belt to that brace.

### 3. Medium — `delete_env` lists every Workspace in the cluster and swallows the error
`crates/workspaces/src/api.rs:1465`:
`if let Ok(list) = wss.list(&ListParams::default()).await { … }`.

Unfiltered, cluster-wide, on every environment delete — and the `Err` arm is silent, so a failed or
partial list leaves workspaces pointing at a deleted environment with no log line at all. Every
other listing in this file goes through `owned_by`/`owned_in` label selectors for precisely this
reason (`api.rs:314-322`, and the comment at `api.rs:313` explaining why).

**Fix:** the attachment is already knowable from a label-free query only because there is no index
on it; add a label (or select `owned_by(&e.spec.owner)`, which covers every workspace that could
legitimately be attached, since `attach_ws` refuses cross-region and the caller must own both) and
log the `Err` arm instead of dropping it.

### 4. Medium — up to ~65 s of blocking syscalls on the reconcile reactor
`bins/agent/src/controller.rs:2006` calls `ensure_shared_home(...)` **directly** on the async
reconcile. It calls `crate::mount_homes` (`lib.rs:110`), which runs
`timeout -s KILL 5 ls <target>` (`lib.rs:75`) and, on a stale mount,
`umount -f -l` plus `timeout -s KILL 60 nsenter … mount` (`lib.rs:171`) — all
`std::process::Command::status()`, all synchronous.

The very next statement (`controller.rs:2008`) correctly wraps `ensure_homecache` in
`spawn_blocking`, and the module doc (`controller.rs:8-13`) states the rule this violates. Same
class, smaller: `write_resolv_conf` (`controller.rs:1685`, called at `2157`) does sync
`create_dir_all`/`read_to_string`/`write` on the reactor on every workspace pass.

**Fix:** `spawn_blocking` both, exactly as line 2008 already does.

### 5. Low — a per-node gate read off a cluster-wide fact
`bins/agent/src/binding.rs:44` (`teams_in_use`, scoped to *this node's* workspaces) vs
`binding.rs:66` (`write_binding_status`, one `NamespaceReady=True` on the single cluster-scoped
`OwnerBinding`) vs `binding.rs:140` (`namespace_ready`, read by every node).

Node A's binding pass sets `NamespaceReady=True` after creating the namespaces *its* workspaces
need. A workspace claimed on node B in a team A has never seen passes the gate at
`controller.rs:1976` before B's own pass has applied that team's namespace, so `ensure_ssh`
(`controller.rs:1611`) fails into the 60 s `RETRY`. Self-healing, but this gate exists to prevent
exactly that. Either make the condition per-node (a `nodeName`-keyed condition or one binding per
(region, owner, node)) or have `namespace_ready` verify the specific namespace exists.

### 6. Low — team slugs are string-interpolated into a label selector
`crates/workspaces/src/api.rs:1739`: `format!("{OWNER_LABEL} in ({})", owners.join(","))`, where
`owners` comes from the directory's `teams_for`. A slug containing `,` or `)` widens or breaks the
selector on a listing that decides `deleted:`. Not reachable today (slugs are directory-validated),
but every other selector in the file takes a single validated value.

---

## Performance

### 7. High — `pull_beat` issues O(V) cluster-wide LISTs per node per beat
Per beat (`WS_REPLICA_SECS`, default 300 s), one node makes:
Nodes ×1 (`peer.rs:324`), VolumeReplicas ×1 (`peer.rs:703`), Workspaces ×1 + Environments ×1
(`peer.rs:794` via `unclaim_kind`), Volumes ×1 (`peer.rs:876`), Volumes ×1 + Workspaces ×1 +
Environments ×1 (`peer.rs:376/399/412`), then Volumes ×1 + VolumeReplicas ×1 + Workspaces ×1 +
Environments ×1 in `retire_pass` (`peer.rs:995/1002/960/967`) —
**and inside the per-volume loop, `pull_volume` lists every `VolumeReplica` in the cluster and
filters client-side** (`peer.rs:501-502`). With V interesting volumes that is V full replica
listings per node per beat, N× that cluster-wide.

The file already knows: `peer.rs:322` lists Nodes once "and threads it through everything below"
for correctness, and `peer.rs:874` carries a ponytail note about the duplicate Volume list.

**Fix:** hoist Volumes, VolumeReplicas, Workspaces and Environments into one listing at the top of
`pull_beat_with` and thread them into `interesting_volumes`, `pull_volume`,
`release_dead_volumes`, `hosted_volumes` and `retire_pass` — the same shape `nodes`/`floor`/`now`
already take. This is one refactor that removes ~9 of the ~13 listings and the O(V) term.

### 8. Medium — a pod LIST per commit per source, inside the pull loop
`bins/agent/src/peer.rs:532` calls `agent_pod_addr` (`peer.rs:235`, a namespaced pod list with two
selectors) inside `for name in order { for &source in &sources {` — a node catching up on N
missing commits does N (× sources) identical lists to learn the same peer IP.

**Fix:** resolve `sources` → addresses once per `pull_volume`, before the loop.

### 9. Medium — `flush_gate` lists every `VolumeReplica` per stop tick
`bins/agent/src/controller.rs:2997`, on the 15 s `TICK` for every stopping workspace/environment.
The comment is right that `.fields("spec.volume=…")` is currently a 400 — but the fix is one line
in the CRD, not client-side filtering forever: `VolumeReplica` already declares
`selectable = ".spec.node"` and `".status.phase"` (`crd.rs:281-282`); adding `".spec.volume"`
makes this (and `peer.rs:501`) a scoped query.

### 10. Medium — retention lists every parent in the cluster on every push
`bins/agent/src/snapshot.rs:215` and `:231` (`worktree_heads`) list all Workspaces and all
Environments, unscoped, and are called from `retain` (`snapshot.rs:303`) after every non-transient
cut — i.e. once per user push. The prefix filter it applies is client-side.

**Fix:** select on `crd::VOLUME_LABEL` (already stamped by `commit_labels`, `crd.rs:693`) or on the
owner label; the same shared parent listing from finding 7 also serves this.

### 11. Low — already-marked walks
`janitor.rs:42` (`dir_bytes("/nix/store")`, a full recursive walk of a ~60 GB store every 10 min)
and `janitor.rs:177` (`warn_oversized_homes`, a recursive walk of every home — over NFS). Both
carry accurate `ponytail:` notes naming the upgrade (`statvfs`, `du --max-depth=1`/qgroups). No
action needed beyond keeping the markers.

---

## Architecture

### 12. Splitting `controller.rs` (3162 lines)
It has five responsibilities with clean, contiguous seams; no logic has to move:

| new file | contents | current lines |
|---|---|---|
| `controller/mod.rs` | `Ctx`, `InFlight`, `Done`, `ReconcileErr`, `Work` | 35-255, 775-790 |
| `controller/run.rs` | `run`, `shutdown_signal`, `heartbeat`, `spawn_*`, `error_policy`, `owned_by`, `wake_stream`, `timed` | 214-560 |
| `controller/status.rs` | `write_status`, `patch_status`, `replace_status`, `settle`, `Outcome`, `conditions_eq`, `settled_status_eq`, `ensure`, `create_if_absent`, `delete_ignoring_404`, `forget_applied` | 885-1068, 3060-3163 |
| `controller/volume.rs` | `reconcile_volume`, `apply_volume`, `volume_work`, `cleanup_volume`, `progressing`, `permanent_reason`, `take_volume`, `ensure_child_volume`, `check_source`, `resolve_volume` | 598-1342 |
| `controller/workspace.rs` | `apply_workspace`, `ensure_profile`, `ensure_ssh`, `stop_workspace`, attach/resolv paths, `pod_is_ready`, `write_ws_status` | 1344-2360 |
| `controller/environment.rs` | `apply_environment`, `run_environment`, `stop_environment`, `restore_gate`, `drain_services`, `writing_pods`, `mkdir_env_mounts`, `write_env_status` | 2362-3058 |
| `controller/stop.rs` | `StopPush`, `stop_push`, `stop_name`, `flush_gate`, `flush_expired`, `flush_timeout`, the three `FlushUnreplicated` constants | 2825-3020 |

`stop.rs` is the one worth pulling out first: it is already the shared half of both parent kinds
and is the most subtle code in the file.

### 13. Duplicated Workspace/Environment pairs
Already factored: `resolve_volume`, `stop_push`, `settle`, `kept_conditions`, `claim::claim`.
Still duplicated:

- **Worktree materialize + `HeadUnknown` guard + `checkout` + `set_quota_worktree`** —
  `controller.rs:2013-2119` vs `controller.rs:2530-2574`. ~60 near-identical lines; the workspace
  copy differs only by `clone_commit` and the worktree name.
- **The "parents on this node, by `volumeRef`" query, written four times** —
  `peer.rs:399/412` (`interesting_volumes`), `peer.rs:960/967` (`hosted_volumes`),
  `sync.rs:76/90` (`live_worktrees`), `snapshot.rs:215/231` (`worktree_heads`).
  One `parents_on_node(ctx) -> Vec<Parent { kind, name, volume, owner, head, phase, pod_ref }>`
  collapses all four **and** is finding 7's fix.
- **`unclaim_kind`'s four per-kind closures** — `peer.rs:731-757`, verbatim twins differing only in
  the status type; `volume_ref_of`/`already_marked_dead` (`peer.rs:853/860`) already prove the
  serde-json approach works for the whole thing.
- **`claim::claim_workspace` / `claim_environment`** — `claim.rs:262` and `:277`, identical `Parts`
  bodies but for the phase.
- **api.rs**: `restore_ws`/`restore_env` (`1118`/`1305`), `push_ws`/`push_env` (`1534`/`1548`),
  `start_ws`/`start_env`, `stop_ws`/`stop_env`.

### 14. Dead code and stale docs
- `Done::lineage_tip` (`controller.rs:189`) — written `None` at its only producer
  (`controller.rs:835`), read nowhere. Delete the field.
- `model::Workspace::live_state` — always `serde_json::Value::Null` (`api.rs:410`), kept only so the
  web's parse is unchanged. Retire it with the next web change.
- `peer.rs:248` documents `WS_PEER_RECV_TIMEOUT_SECS`; no such variable exists anywhere in the repo.
- One-implementation traits: `nix::Nix`, `api::Directory`, `store::MetaStore` — all genuine test
  seams with real fakes in the suites. Leave them.
- Config knobs: every `WS_*` knob I checked is set in `deploy/k3s/agent-daemonset.yaml`
  (`WS_RUNTIME_CLASS`, `WS_GIT_SSH_PORT`, `WS_GIT_INIT_IMAGE`, `WS_PEER_ADDR`, `WS_SNAPSHOT_KEEP`,
  `WS_REPLICA_SECS`, `WS_SYNC_SECS`, `WS_STOP_FLUSH_TIMEOUT_SECS`, `WS_NODE_DEAD_SECS`,
  `WS_PEER_SEND_TIMEOUT_SECS`). No unused knobs found.

---

## Tests: load-bearing paths with none

Coverage is strong (`reconcile.rs` is 3470 lines and covers claim races, takeover, stop flush,
restore gating, retire/reap, profile builds). These are the gaps, highest value first:

1. **`k8s::attach_egress` / `attach_ingress`** (`k8s.rs:1291`, `1312`) — nothing asserts that
   `namespaceSelector` and `podSelector` sit in ONE `from`/`to` peer. The functions' own comments
   say the two-peer form lets "any pod in the cluster … reach every sshd" and admits "another
   owner's workspace that happens to share the same id". This is the single most valuable untested
   invariant in the crate, and it is a pure-function assertion on JSON.
   Same for `allow_gateway_ingress` (`k8s.rs:1259`) and `allow_internet_egress`'s
   `CLUSTER_INTERNALS` except-list (`k8s.rs:1223` — the metadata-service block).
2. **`k8s::workspace_pod` name handling** — no test feeds it a hostile `spec.name`; see finding 1.
3. **`controller::write_resolv_conf`** (`controller.rs:1685`) — the "written IN PLACE, never
   renamed" inode invariant, which the doc comment marks "do not 'fix' this into an atomic write".
   A test asserting the inode number survives a rewrite is the only thing that will stop that
   refactor.
4. **`controller::mkdir_env_mounts`** (`controller.rs:3024`) — `validate_mount` is tested in
   `model.rs`, but nothing asserts the *controller* refuses a traversing folder before
   `create_dir_all`, which is where the escape actually lives.
5. **`janitor::drop_stale_worktrees`** (`janitor.rs:287`) — deletes worktree subvolumes on any
   non-owner node; no test anywhere. The empty-owner and `owner == me` guards are the whole safety
   argument and are unasserted.
6. **`snapshot::retain`'s transient arm** (`snapshot.rs:283-296`) — "delete every other transient of
   this worktree"; nothing asserts it spares a non-transient commit or another worktree's sync point.
7. **`binding::teams_in_use` / `namespace_ready` interaction** — finding 5's race.

---

## Architecture notes

1. **One shared "parents and volumes on this node" listing, threaded through the beats.** It is
   simultaneously the fix for the O(V) API-server fan-out (finding 7), the pod-list-per-commit
   (8), retention's cluster scans (10), and four copies of the same query (13). Biggest payoff of
   anything here, and it is a refactor of listing plumbing only — no decision logic moves.
2. **Split `controller.rs` along the seven seams in the table above, starting with `stop.rs` and
   `status.rs`.** Both are already kind-agnostic shared code with their own invariants; separating
   them makes the workspace/environment files small enough that the remaining duplication (the
   ~60-line worktree materialize block) becomes obvious enough to factor.
3. **Make untrusted-CR validation uniform.** `git_init_container` and `service_statefulset`
   re-validate at the agent and say why; `workspace_pod` and every consumer of `spec.owner` do not.
   One `validate_spec(&WorkspaceSpec)` called at the top of `apply_workspace` (name, owner, team,
   packages) closes findings 1 and 2 together and puts the rule in one readable place instead of
   relying on a label patch to fail first.
