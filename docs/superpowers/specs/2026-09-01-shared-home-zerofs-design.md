# Shared home on ZeroFS — design

Date: 2026-09-01. Status: approved in discussion, ready for planning.

## Problem

Every person has one persistent home per region, pinned to ONE node: the `OwnerBinding` records
`spec.node_name`, `bound_elsewhere` refuses any claim on another node, and the home `Volume` is a
btrfs subvolume that only exists on that node. Consequence: an owner's workspaces can never be
scheduled onto another node, even when that node's `VolumeReplica` rows are all Synced — the home
pin, not replica sync, is the binding constraint. A dead binding node strands the owner entirely
(nothing repoints an `OwnerBinding`).

## Decision

Move the home's CONFIG content to a region-shared POSIX filesystem — ZeroFS (S3-backed, SlateDB
LSM, served over NFS) — mounted once per node by the agent. Keep everything performance-sensitive
node-local. Delete the home-as-btrfs-Volume machinery and the owner→node pin.

Explicitly accepted losses (ruled by the owner of this repo, 2026-09-01):

- **Home history is gone.** No commits, no snapshots, no restore for the home. Durability is
  "S3 is durable", nothing more.
- **Per-home quota is gone.** No qgroup on the shared side. A janitor size alarm replaces it
  (below), not a hard cap.

## Layout

```
/home/kl                      ← NFS   {ZeroFS mount}/{owner}          shared, every node
/home/kl/.local-cache         ← local btrfs  {pool}/homecache/{owner}  per (owner, node), disposable
/home/kl/.vscode-server       ← local btrfs  {pool}/homecache/{owner}/.vscode-server
/home/kl/.cursor-server       ← local btrfs  {pool}/homecache/{owner}/.cursor-server
/home/kl/.local/state         ← local btrfs  {pool}/homecache/{owner}/.local-state
/home/kl/workspaces/<name>    ← local btrfs  {pool}/vol/{volume}/live/{ws}   (unchanged)
```

- The shared side is the WHOLE home minus carve-outs — an unknown dotfile a tool invents lands on
  NFS and just works. No allow-list.
- Caches are redirected by ENV, not enumerated by path: `XDG_CACHE_HOME`, `npm_config_cache`,
  `PNPM_STORE_DIR`, `BUN_INSTALL_CACHE`, `CARGO_HOME`, `RUSTUP_HOME`, `GOMODCACHE`, `GOPATH`,
  `GRADLE_USER_HOME`, `UV_CACHE_DIR`, `PIP_CACHE_DIR`, `DENO_DIR`, `PLAYWRIGHT_BROWSERS_PATH` —
  all under `/home/kl/.local-cache/…`. A tool honouring `XDG_CACHE_HOME` is handled without being
  named. Path mounts remain ONLY for the two editors' server dirs, which offer no env var.
- The cache subvolume is disposable BY CONTRACT: the janitor may delete it whenever the pool is
  tight; the cost is a re-download. No quota, no replication, no history.
- **Append-mode files are local** (vetting finding 3): NFS `O_APPEND` is not atomic, so shell
  history and `~/.local/state` (SQLite) go on the local side. `HISTFILE` must be set explicitly
  to the local state dir — `ZDOTDIR` already points zsh at `{HOME_DIR}/.config/zsh`, which is the
  SHARED side, so without an explicit `HISTFILE` the default shell corrupts its history on day
  one. Bash gets the same `HISTFILE`.

## ZeroFS deployment

One ZeroFS process per region — SlateDB is single-writer (writer-epoch fencing, the same
invariant as this platform's repo databases), so one instance per bucket prefix, never one per
node. Nodes are plain NFS clients of it.

- Runs as a k8s Deployment, one replica, in `rustic-git-system`, resource requests pinned.
  Ruled: accept the availability window — if it is down, `/home/kl` hangs (hard NFS mount) on
  every node until it reschedules. Today a node failure costs one node's workspaces; this trades
  that for a region-wide but short hang. Revisit only if it hurts in practice.
- Backing store: the region's existing Azure blob account, its own container. Credentials live
  with the Deployment, never per owner and never in workspace pods.
- The AGENT mounts the export once per node at `{pool}/homes` (it is already privileged and
  root). Pods hostPath `{pool}/homes/{owner}` at `/home/kl`. No CSI, no PV, no per-owner Secret —
  the same convention-and-folders model the PV deletion established.
- fsync caveat, documented not solved: NFS COMMIT may return before durability. For configs
  (small, whole-file, write-temp-then-rename) this window is acceptable. 9P would close it but
  does not compose with gVisor pods; not pursued.

## Provisioning order (vetting finding 2)

The seeder init container and sshd both run before anything else touches the home, and root in
the pod chowns exactly one path. So the AGENT creates `{pool}/homes/{owner}` (uid 1000) and the
`{pool}/homecache/{owner}` subvolume during the workspace's own reconcile, before the pod is
applied — replacing today's "home Volume Ready" gate. First pod on a node: cache is cold (npm
re-downloads, editors reinstall their server — minutes, once). First pod for an owner anywhere:
empty home.

## The OwnerBinding survives, de-pinned (vetting finding 1)

Deleting `bound_elsewhere` alone does NOT free scheduling — the binding also creates the owner's
namespaces (`ws-{owner}`, team namespaces), the claim's `ensure_binding` depends on it, and the
workspace reconcile parks on it. The binding is kept as the owner's NAMESPACE ensurer:

- `spec.node_name` loses all meaning. It stays in the schema (existing objects must parse) but
  nothing reads it; `bound_elsewhere` is deleted; `claim::ensure_binding` keeps creating the
  binding on a claim win, from any node.
- The binding reconciler runs on EVERY node's agent (today: only the bound node's). Its objects
  are cluster-global (namespaces, LimitRanges), so the reconcile must be convergent under two
  nodes running it concurrently — server-side apply / create-ignoring-AlreadyExists, no
  read-modify-write.
- `spec.home_quota_gb` becomes dead (no qgroup on NFS). Stays in the schema, ignored.
- Result: the claim admits any node whose `VolumeReplica` is Synced (the `may_claim` rule,
  unchanged) — which was the point of all this.

## Deletions, in dependency order (vetting finding 4)

The stop path currently FAIL-CLOSED waits on a `stop-home-{ws}` snapshot before deleting the pod;
removing the beat before rewriting that arm hangs every stop forever. Order is load-bearing:

1. **Stop path first**: drop the `stop-home-{ws}` gate from workspace stop (the home no longer
   needs a push to be safe — NFS is the durable copy at all times). Environment stop keeps its
   own `stop-{env}` gate untouched (that volume is still commit-model).
2. **Then the home push machinery**: `spawn_home_push`, `homes_to_push`, `home_push_interval`,
   `push_env`'s home half, `pushed_generation`/`record_pushed_gen`/`.pushed-gen` files.
3. **Then materialize**: `home_target` in `volume_work`, `materialize_home`,
   `HOME_AWAITING_SYNC`, `HomeNotReady` (replaced by the agent's mkdir in "Provisioning order"),
   `ensure_home_dirs` (nested subvolumes are meaningless on NFS).
4. **Then the CRD surfaces**: `ensure_home` in `binding.rs`, `is_home_volume` branches in the
   volume reconciler / claim / snapshot reconciler / pull beat, `home_volume_name` callers,
   `crd::HOME`-related admission rule in `agent-admission.yaml` (the `Volume.spec.quotaGb`
   exception for OwnerBinding-owned volumes), the home row injected into the volumes listing at
   `api.rs:1782` (UI would list a phantom otherwise).
5. **Last, data**: migrate then delete the existing home Volume CRs and subvolumes (below).

## Migration

Per owner, at cutover, on the binding's node: stop the owner's pods, `rsync -a` the btrfs home's
content (minus the nested cache subvolumes) into `{pool}/homes/{owner}`, restart. Then delete the
`home-{owner}` Volume CR (ownerReference cleanup follows) and the subvolume. This cluster has one
real owner (`karthik1729`) plus e2e leftovers; the spec records the general procedure, the
rollout does it once by hand.

## Safety nets

- **Size alarm** (replaces quota): janitor logs a warning when an owner's shared home exceeds
  100 MB. Configs never will; the alarm firing means a cache escaped the redirection and is
  landing on S3 — caught from a log line, not a bill or a slow workspace.
- **Optional, declined for now**: periodic tarball of the shared home to blob for point-in-time
  dotfile restore. Cheap (single-digit MB), but ruled out with home history; revisit on demand.

## What this does NOT change

- Workspace/environment volumes, worktrees, commits, replication, restore — untouched. The
  commit model remains the story for everything except the home.
- Cross-region: each region has its own ZeroFS and bucket; homes still do not sync across
  regions (unchanged from today, and now trivially fixable later by pointing two regions at one
  bucket — NOT done here, single-writer would forbid it anyway).
- `tests/ws_e2e.sh` gains a ZeroFS prerequisite (exit 77 without it), same pattern as btrfs/k3s.

## Open items for the plan

- Exact ZeroFS version pin, container image, and its SlateDB bucket layout.
- Benchmark on build-0 before rollout: cold/warm `npm install` against the redirected local
  cache, VS Code server start, and a plain `ls -la ~` over NFS — regression gates, not tuning.
- The `unclaim_dead_nodes` sweep already un-places workspaces from dead nodes; with the pin gone
  they become claimable by survivors with Synced replicas — verify that end to end (it is the
  auto-heal this whole change buys).
