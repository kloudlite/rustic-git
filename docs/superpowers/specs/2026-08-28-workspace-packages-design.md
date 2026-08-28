# Workspace packages: a host Nix store, one profile per workspace

**Status:** design, awaiting review. **Depends on:** controller ownership
(`2026-08-27-controller-ownership-design.md`) — the Workspace reconciler, the `Volume` child, the
per-node agent.

## Problem

A workspace is an image plus a subvolume. Everything a developer needs beyond the image — a
toolchain, a database client, a CLI — is either baked into a custom image (slow to change, one
image per combination) or installed by hand into `/workspace` (lost on a fresh clone, not
reproducible, and `apt` inside a read-only-rootfs pod does not work anyway). We want packages to
be a property of the workspace: declared, reproducible, present on the next node the workspace
lands on, and cheap to change.

## Decision, in one paragraph

Packages are **declarative and live on the subvolume**: the list is a file in the workspace,
`/workspace/.kloudlite/packages.json`, so it is part of every snapshot, clone and restore
exactly like the code next to it, and a developer can edit it from inside the workspace (and
commit it, if they want the repo to carry it). Each node runs a **Nix store and daemon on the
host** (`/nix`, installed and run by the agent DaemonSet — a second container, not a host
provisioning step). On every pass — and therefore at every startup, restore, clone and move —
the Workspace reconciler reads that file, realises anything missing through the daemon, and
builds **one profile per workspace** — a `buildEnv` whose out-link is a GC root at
`/nix/var/rustic/profiles/{id}` — *before* the pod is (re)started, the same way the subvolume
must exist before the pod. The pod mounts `/nix/store` and its own profile **read-only** through
a local PersistentVolume (never a hostPath — PSA `baseline` stays), and the profile's `bin` is on
`PATH`. The API and the web edit the list too, but never by writing the subvolume: `/v1` records
a **write request** on the spec (`spec.packages` + `packagesRequestedAt`), and the owning node's
reconciler applies it to the file once — after that the file is the truth again. Changing the
list rebuilds the profile and swaps the out-link atomically; a running pod sees the new tools
without a restart. Nothing inside a pod can write to the store or reach the daemon.

Why the file and not the spec: a snapshot is the whole state of a workspace, and "restore
`third`" must bring back the tools `third` was made with, not whatever the spec says today. The
spec is a CRD in one cluster; the subvolume is what travels.

Not chosen: an imperative `nix profile install` from inside the workspace against the host
daemon. It needs the daemon socket in the pod (a hostPath, and a root-equivalent on the host).
It can be added later as a power-user option without changing anything below — the file would
simply be updated by that tool as well.

## Components

| Component | Where | What it does |
|---|---|---|
| `nix-daemon` container | `rustic-git-agent` DaemonSet, kube-system, one per node | Runs `nix-daemon` from the `nixos/nix` image with the host's `/nix` mounted. The store, the SQLite db, the daemon socket and every profile live on the host disk under `/nix`; the container is stateless. |
| Nix client in the agent image | `bins/agent` image | `nix` CLI, talking to the daemon over `/nix/var/nix/daemon-socket/socket` (both containers mount host `/nix`). The agent never runs a build itself: the daemon does, as `nixbld` users, sandboxed. |
| `packages` module | `crates/workspaces/src/packages.rs` | Pure: validates attribute names, renders the `buildEnv` expression, derives the profile hash. Testable without Nix. |
| Profile step in the Workspace reconciler | `bins/agent/src/controller.rs` | Between "volume materialized" and "pod applied": realise, then link. Writes `status.packages`. |
| Nix PV/PVC | `crates/workspaces/src/k8s.rs` | `nix_pv(id)` / `nix_claim(ns, id)`: a read-only local PV over host `/nix`, one per workspace (a local PV binds to exactly one PVC), mounted twice by subPath. |
| Janitor | `bins/agent/src/lib.rs` janitor beat | `nix-collect-garbage` when the store exceeds a threshold; roots are the profile out-links, so nothing a workspace uses is ever collected. |
| `/v1` + web | `crates/workspaces/src/api.rs`, workspace create/settings | `packages` on create and on `PATCH /v1/workspaces/{id}`; a Packages field in the create dialog and a Packages section in workspace settings. |

## Data model

### The file (the truth)

`/workspace/.kloudlite/packages.json` on the workspace's live subvolume:

```json
{ "nixpkgs": "github:NixOS/nixpkgs/a1b2c3…", "packages": ["nodejs_20", "go", "postgresql_16"] }
```

- `packages`: nixpkgs attribute names. Each matches `^[A-Za-z0-9_][A-Za-z0-9_.+-]*$`, ≤ 64 chars,
  ≤ 100 entries, no duplicates. Dotted attributes (`python3Packages.requests`) pass through.
- `nixpkgs`: the pin the list was written against. Written by the reconciler when it applies a
  request (from the agent's `WS_NIXPKGS`); a developer editing the file by hand may leave it —
  then the agent's pin is used. Carried so a snapshot taken under one pin still builds the same
  tools under a later one if the pin is honoured (release 1 honours it: the expression uses the
  file's pin when present, the agent's otherwise).
- Missing file = no packages. A file that does not parse, or fails validation, is **not** an
  empty list: the profile stays as it was and `PackagesReady=False/InvalidFile` names the line.
  The reconciler reads a file from a user-writable subvolume, so it is untrusted input: 64 KB cap,
  strict JSON, the grammar above, nothing else interpreted.

### CRD

```yaml
spec:
  packages: ["nodejs_20", "go"]              # a WRITE REQUEST from /v1, optional
  packagesRequestedAt: "2026-08-28T10:00:00Z" # what makes a repeat of the same list a new request
status:
  packages:
    observed: ["nodejs_20", "go"]            # what the FILE says, as of the last pass
    observedHash: "sha256:…"                 # of (nixpkgs, sorted packages) — what the profile on disk IS
    profile: "/nix/var/rustic/profiles/ws-…"
    nixpkgs: "github:NixOS/nixpkgs/<rev>"
    appliedRequestAt: "2026-08-28T10:00:00Z" # the request already written to the file
  conditions:
    - type: PackagesReady   # True/Built | False/Building | False/BuildFailed | False/InvalidFile | False/NoNix
```

- `spec.packages` is validated at the API with the same grammar. It is a request, not state:
  the reconciler writes it into the file exactly once (guarded by
  `packagesRequestedAt != status.packages.appliedRequestAt`, the `RestoreWish` pattern), and from
  then on the file wins — including over the spec, which still shows the old request. The
  projection the UI reads is `status.packages.observed`, never `spec.packages`.
- `observedHash` is the idempotency key for the *profile*. A pass whose hash of the file equals
  it and whose out-link exists does nothing. A changed file, a changed pin, or a missing out-link
  (a fresh node after a move; a fresh subvolume after a restore or clone) each rebuild.
- The nixpkgs pin is **per agent** (`WS_NIXPKGS`); the file records the one it was written under.

### Host layout (`/nix`, owned by root, world-readable)

```
/nix/store/…                              the store
/nix/var/nix/db, daemon-socket/socket     Nix's own
/nix/var/rustic/profiles/{ws-id}          out-link → /nix/store/…-ws-{id}-env   (GC root)
/nix/var/rustic/profiles/{ws-id}.building  out-link of an in-flight build, renamed over the above
```

`nix build -o` creates the out-link and registers it under `/nix/var/nix/gcroots/auto`; the
rename is the atomic publish. Deleting the workspace deletes the out-link (the Volume finalizer
already runs on the owning node); the next GC frees whatever nothing else references.

## Reconcile

In `reconcile_workspace`, after `resolve_volume` returns `Ready` and before the pod is ensured.
This runs on **every** pass, which is what "install missing packages at startup" means here:
a restore, a clone, a move to another node, or an agent restart all arrive at step 1 with a
file whose hash does not match the profile on this node, and the profile is rebuilt before the
pod starts.

0. **Apply a pending request.** If `spec.packagesRequestedAt` is set and differs from
   `status.packages.appliedRequestAt`: write `{nixpkgs: WS_NIXPKGS, packages: spec.packages}` to
   the file (create `.kloudlite/`, write a temp file, rename — atomic, and on the subvolume so
   the very next snapshot carries it), then record `appliedRequestAt`. The file is written from
   the agent (root on the host) with the subvolume's owning uid/gid, so the developer can still
   edit it. On a blocking thread, like every subvolume write.
1. **Read the file** (untrusted; see Data model). Invalid → `InvalidFile`, keep the old profile,
   `await_change` (a file only changes with a pod write or a request, both of which wake the
   pass — the pod write via the 15 s tick only; see "Watching the file").
   `status.packages.observed` ← the list.
2. `hash = packages::hash(pin, &list)`. If `status.packages.observedHash == hash` and the
   out-link exists → skip to 6.
3. Empty list → the empty `buildEnv` (still built: an empty profile is a valid, mountable
   directory, and "no packages" must not be a special case in the pod spec).
4. Realise, on a blocking thread (`nix` blocks; the pattern is the btrfs work's):
   ```
   nix build --no-link --print-out-paths --impure \
     --expr '(import (builtins.getFlake "<pin>") { }).buildEnv {
               name = "ws-<id>-env"; paths = [ pkgs.nodejs_20 pkgs.go … ]; }' \
     -o /nix/var/rustic/profiles/<id>.building
   ```
   Rendered by `packages::expression`, never by string-formatting user input into a shell: the
   attribute names are validated, and they are passed as a Nix list literal, not interpolated
   into a command line. `nix` is exec'd with an argv, not a shell. Bounded by a deadline
   (`WS_NIX_TIMEOUT`, default 20 min) — a stuck substituter must not hold the reconciler.
5. On success: `rename(<id>.building, <id>)`; write `status.packages` and
   `PackagesReady=True/Built`. On failure: leave the previous profile (if any) in place, write
   `PackagesReady=False/BuildFailed` with the last 20 lines of stderr as the message, and
   requeue at `RETRY`; the pod is still applied with the previous profile if one exists,
   otherwise the workspace waits (a pod whose PATH points at a missing profile is worse than a
   `Creating` workspace with a clear condition). The condition text is what the UI shows — an
   attribute that does not exist reads `error: attribute 'nodejs_99' missing` verbatim.
6. Ensure `nix_pv(id)` and `nix_claim(ns, id)` alongside the live PV/PVC; apply the pod.

While a build runs the pass returns `requeue(TICK)` with `PackagesReady=False/Building` — the
wake-on-finish channel the snapshot reconciler uses is reused, so completion is an event, not a
tick.

`Resolved::Wait` for the volume stays first: no file is read and no profile is built for a
workspace whose disk does not exist yet, so a placement failure does not cost a build.

**In-place restore** already scales the pod down before the disk is swapped (`restore_gate`);
the pass that scales it back up goes through steps 1–6, so a restored subvolume's file is what
the pod comes up with. Same for clone (first pass on the new subvolume) and move (first pass on
the new node).

### Watching the file

A developer editing `packages.json` inside the pod produces no Kubernetes event. Release 1
picks it up on the Workspace's 15 s tick (the reconciler already requeues on it) — the profile
swap is atomic, so a tool appears on `PATH` within a tick of saving the file. ponytail: an
inotify watch on `{live}/.kloudlite/packages.json` from the agent (it is on the host) feeding the
wake channel is the upgrade if the tick is felt.

## Pod

Two additional mounts on the workspace container, both `readOnly: true`, from one PVC
`nix-{id}`:

| subPath (under host `/nix`) | mountPath | why |
|---|---|---|
| `store` | `/nix/store` | profile symlinks are absolute into the store |
| `var/rustic/profiles/{id}` | `/nix/profile` | the workspace's own profile only — not other workspaces', not the daemon socket |

Environment: `PATH=/nix/profile/bin:$PATH` (prepended via the container's `env`; the image's own
`PATH` is unknown, so the entrypoint's `PATH` is extended by `sh -c` only when the image has no
`PATH` env — see `packages::path_env`, tested). Also `NIX_PROFILE=/nix/profile` for tools that
want to know, and `MANPATH`/`XDG_DATA_DIRS` extended the same way for man pages and completions.

The nix PV is **not** a hostPath in the pod: the PV object names the host path, the pod names a
claim, exactly as `live` does — `hardened()` and the PSA `baseline` test
(`workspace_pod_has_no_host_path`) keep holding. The PV carries `readOnly: true` on the local
source and the claim is `ReadOnlyMany`; the mounts are read-only on top. Store paths are
world-readable by Nix's own design; a workspace can read any store path, which is the same
guarantee as any Nix machine.

## Host daemon

The `nix-daemon` container in the agent DaemonSet:

- image `nixos/nix:<version>`, command `nix-daemon`, privileged (needs user namespaces for
  the build sandbox; the DaemonSet is already privileged), `hostPath /nix` mounted at `/nix` with
  `DirectoryOrCreate` — the store is created on first run, no host provisioning.
- `/etc/nix/nix.conf` from a ConfigMap: `experimental-features = nix-command flakes`,
  `substituters = https://cache.nixos.org`, `trusted-public-keys` for it, `max-jobs = 2`,
  `cores = 2`, `min-free`/`max-free` so the daemon itself keeps headroom. Nothing user-tunable.
- The agent container mounts the same hostPath and sets `NIX_REMOTE=daemon`. It runs `nix` as
  root (it already is), which the daemon treats as a trusted user — acceptable because the agent
  is the only client, and the expression it evaluates is rendered from validated input.
- Liveness: the daemon container's own probe (`nix store ping`). A node whose daemon is down
  reports `PackagesReady=False/NoNix` on its workspaces and requeues; pods with an existing
  profile keep running.

Disk: the store grows with the union of everything ever built on that node. The janitor beat
runs `nix-collect-garbage` when `/nix` exceeds `WS_NIX_GC_HIGH` (default 60 GB) down to
`WS_NIX_GC_LOW`; profile out-links are the only roots, so GC is always safe. Per-node, not global
— a store is a node's cache, the spec is the truth.

## Moves, clones, restores, snapshots

The file is on the subvolume, so every one of these carries it for free — and the reconcile above
rebuilds the profile from it before the pod comes back:

- **Snapshot:** the file is in the btrfs snapshot like everything else under `/workspace`. A
  snapshot taken with `go` in it restores with `go` in it, whatever the spec or the node say.
- **Restore (in place or into a new workspace):** the restored subvolume's file wins. The
  reconciler's first pass sees a hash that does not match this node's profile and rebuilds. An
  old `spec.packages` request is NOT re-applied: `appliedRequestAt` already equals
  `packagesRequestedAt`.
- **Clone:** the clone's subvolume has the file; its first pass builds its own profile.
- **Move / new node:** `observedHash` matches but the out-link is missing → rebuild from the
  file. Substitutes make this a download, not a compile.
- **Delete:** the Workspace finalizer removes the out-link (and `.building`). The nix PV/PVC are
  children by ownerReference, gone with the object.

## API and web

- `POST /v1/workspaces` accepts `packages: string[]` — written as the first request
  (`packagesRequestedAt = now`), so the file exists before the pod ever starts. A workspace
  created from a repo whose checkout already contains `.kloudlite/packages.json` and whose
  request is empty keeps the repo's file: the reconciler applies a request only when
  `spec.packages` is non-empty or the request explicitly says `replace: true` (the settings
  page's "Apply" always does).
- `PATCH /v1/workspaces/{id}` with `{ "packages": [...] }` writes a new request
  (`packagesRequestedAt = now`, validated as above). Spec only; nothing about the node is
  consulted; the owning node applies it to the file.
- `GET /v1/workspaces/{id}` projects `packages` from **`status.packages.observed`** (the file)
  and `packagesStatus` (`ready`, `hash`, `message`, `pendingRequest: bool`) from the condition
  and the request/applied timestamps.
- Web: the create-workspace dialog gets a **Packages** chip input (free text, one attribute per
  chip, validated client-side with the same regex; a hint links to search.nixos.org). Workspace
  settings gets a **Packages** section with the same input, "Apply" → PATCH; the workspace row and
  header show `PackagesReady` while building/failed, with the condition message on hover — a
  misspelled attribute must be visible without opening the pod.

No package search in v1: the attribute list is ~100k entries and changes with the pin;
search.nixos.org is the reference. (ponytail: a cached `nix search --json` per agent is the
upgrade if free text proves error-prone.)

## Security

- Pods: no daemon socket, no writable store, no hostPath, PSA `baseline` unchanged. A workspace
  can read `/nix/store` and its own profile; it cannot see the profile directory of another.
- The file is user-writable and read by root on the host: it is parsed as data only — 64 KB
  cap, strict JSON, the grammar for every attribute, `nixpkgs` must be a `github:NixOS/nixpkgs/`
  ref with a 40-hex rev (anything else is `InvalidFile`, not "use it"). A workspace cannot make
  the agent evaluate arbitrary Nix.
- Agent: the only Nix client. Attribute names are validated at the API, at the file, **and**
  again in `packages::expression` before rendering; the expression is passed as one argv element; `nix` is
  exec'd, never via a shell. A hostile attribute name cannot become code: the grammar excludes
  quotes, spaces, `$`, `(`, `;`.
- Builds run in the daemon's sandbox as `nixbld` users, with network only for fixed-output
  derivations, as Nix itself enforces. A malicious *package* is the same risk as a malicious
  image — it runs inside the workspace pod, under the pod's own security context.
- Substituter: `cache.nixos.org` with its signing key only. No user-supplied substituters or
  keys.

## Failure modes

| Failure | Behaviour |
|---|---|
| Attribute does not exist | `PackagesReady=False/BuildFailed`, nix's own message; previous profile stays; requeue `RETRY`. |
| Substituter unreachable | build compiles from source or fails on the deadline; same condition; retry. |
| Daemon down | `NoNix`; pods with a profile keep running; new workspaces wait. |
| Agent restarts mid-build | `.building` out-link may exist; the next pass rebuilds (idempotent — Nix's store is content-addressed, the rebuild is a cache hit); `.building` is replaced. |
| `/nix` full | daemon's `min-free` triggers GC; if still full, `BuildFailed` with the disk message. |
| Workspace moved / restored / cloned | rebuild on the node from the file (see above). |
| File edited to something invalid | `InvalidFile` with the line; previous profile stays; fixed on the next tick after the file is fixed. |
| Request and hand-edit race | the request is written once; the developer's later edit wins, as any later write does. |

## Testing

- `packages.rs` unit tests: regex accept/reject table (including `$(…)`, quotes, spaces, `..`),
  expression rendering is byte-exact for a fixed input, hash is order-independent and
  pin-sensitive, `path_env` for images with and without `PATH`.
- `k8s.rs` tests: the workspace pod has the two read-only mounts and no hostPath (extends the
  existing PSA test); `nix_pv` is read-only with node affinity.
- Agent reconcile tests (mocked API server + a fake `nix` runner): build-then-apply ordering,
  skip on matching hash, `BuildFailed` keeps the pod on the old profile, missing out-link rebuilds.
- `packages.rs` file tests: the reader rejects oversize, non-JSON, bad attributes, a foreign
  `nixpkgs` ref; the writer is atomic (temp + rename) and preserves the subvolume's uid/gid.
- `tests/ws_e2e.sh`: a workspace with `packages: ["hello"]`; `kubectl exec … hello` prints
  "Hello, world!"; PATCH to `["hello", "jq"]`; without a pod restart `jq --version` works;
  edit the file from inside the pod to add `tree`; within a tick `tree --version` works; take a
  snapshot; PATCH to `[]` (`hello` gone); restore the snapshot in place; `hello`, `jq` and
  `tree` are back with no request involved.

## Rollout

1. CRD: add `spec.packages`, `spec.packagesRequestedAt` and `status.packages` — additive, apply
   first. Existing workspaces have no file → no packages → an empty profile.
2. Agent DaemonSet: add the `nix-daemon` container, the `/nix` hostPath, the nix.conf ConfigMap,
   `WS_NIXPKGS`; agent image gains the `nix` client. Roll. Existing workspaces: `packages` empty →
   an empty profile is built on their next pass; their pods are NOT restarted for it (the mounts
   are added on the next pod apply, which happens on the next spec change or move). ponytail: a
   pod created before the roll has no `/nix` until it is recreated; acceptable for release 1.
3. API + web.

## Out of scope (deliberately)

- Imperative `nix` inside the workspace, user flakes, per-team nixpkgs pins, private binary
  caches, non-nixpkgs sources. Each is additive on top of this design.
- Packages for environment services: services are images; that is what images are for.
