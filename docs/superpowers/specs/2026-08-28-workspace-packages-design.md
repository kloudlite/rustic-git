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

Packages are **declarative on the Workspace object**: `spec.packages` is a list of nixpkgs
attribute names, written by `/v1` (create, and `PATCH /v1/workspaces/{id}`), edited in the web
UI, pinned to one nixpkgs revision per agent. Each node runs a **Nix store and daemon on the
host** (`/nix`, installed and run by the agent DaemonSet — a second container, not a host
provisioning step). On every pass — and therefore at every startup, restore, clone and move —
the Workspace reconciler hashes the spec's list, realises anything missing through the daemon,
and builds **one profile per workspace** — a `buildEnv` published as `/nix/var/rustic/profiles/{id}/current`, under one indirect GC root — *before* the pod is (re)started, the same way the subvolume
must exist before the pod. The pod mounts `/nix/store` and its own profile **read-only** through
a local PersistentVolume (never a hostPath — PSA `baseline` stays), and the profile's `bin` is on
`PATH`. Changing the list rebuilds the profile and swaps that link atomically; the running pod
sees the new tools without a restart. Nothing inside a pod can write to the store or reach the
daemon.

Why the spec and not a file in the repository: a repo has branches, and "which branch's file,
and what about the working copy?" has no answer that is both stable and editable. The Workspace
is one object with one list; a clone copies it; a restore does not touch it (snapshots carry
data, not configuration — the object carries that).

Not chosen: `kloudlite.yaml` in the checkout as the truth (the branching problem above), and an
imperative `nix profile install` from inside the workspace against the host daemon (needs the
daemon socket in the pod, a hostPath and a root-equivalent on the host; and the result would
live outside the spec). Either can be added later without changing anything below.

## Components

| Component | Where | What it does |
|---|---|---|
| `nix-daemon` container | `rustic-git-agent` DaemonSet, kube-system, one per node | Runs `nix-daemon` from the `nixos/nix` image with the host's `/nix` mounted. The store, the SQLite db, the daemon socket and every profile live on the host disk under `/nix`; the container is stateless. |
| Nix client in the agent image | `bins/agent` image | `nix` CLI, talking to the daemon over `/nix/var/nix/daemon-socket/socket` (both containers mount host `/nix`). The agent never runs a build itself: the daemon does, as `nixbld` users, sandboxed. |
| `packages` module | `crates/workspaces/src/packages.rs` | Pure: validates attribute names, renders the `buildEnv` expression, derives the profile hash. Testable without Nix. |
| Profile step in the Workspace reconciler | `bins/agent/src/controller.rs` | Between "volume materialized" and "pod applied": ping the daemon, realise, then link. Writes `status.packages`. |
| Nix PV/PVC | `crates/workspaces/src/k8s.rs` | `nix_pv(id)` / `nix_claim(ns, id)`: a read-only local PV over host `/nix`, one per workspace (a local PV binds to exactly one PVC), mounted twice by subPath. |
| Janitor | `bins/agent/src/lib.rs` janitor beat | `nix-collect-garbage` when the store exceeds a threshold; the indirect root over the profiles tree means nothing a workspace uses is ever collected. |
| `/v1` + web | `crates/workspaces/src/api.rs`, workspace create/settings | `packages` on create and on `PATCH /v1/workspaces/{id}`; a Packages field in the create dialog and a Packages section in workspace settings. |

## Data model

### CRD

```yaml
spec:
  packages: ["nodejs_20", "go", "postgresql_16"]   # nixpkgs attribute names, optional, default []
status:
  packages:
    observed: ["nodejs_20", "go"]            # the list the profile on disk was built from
    observedHash: "sha256:…"                 # of (nixpkgs, sorted packages) — what the profile IS
    profile: "/nix/var/rustic/profiles/ws-…"
    nixpkgs: "github:NixOS/nixpkgs/<rev>"    # the pin the profile was built with
  conditions:
    - type: PackagesReady   # True/Built | False/Building | False/BuildFailed | False/NoNix
```

- `spec.packages`: each entry matches `^[A-Za-z0-9_][A-Za-z0-9_.+-]*$`, ≤ 64 chars, ≤ 100
  entries, no duplicates. Dotted attributes (`python3Packages.requests`) pass through. Validated
  at the API (422 with the offending entry) **and** again by the reconciler before rendering —
  an object written by any other path (kubectl, a restored backup) is not trusted to have been
  validated. The API does not know whether an attribute exists; the reconciler does, and says so
  in the condition.
- `observedHash` is the idempotency key for the *profile*. A pass whose hash of the spec equals
  it and whose `current` link exists does nothing. A changed list, a changed pin, or a missing profile
  (a fresh node after a move) each rebuild.
- The nixpkgs pin is **per agent** (`WS_NIXPKGS`, a flake ref with a 40-hex rev). Rolling it is
  an agent redeploy; every workspace on that node rebuilds on its next pass (a cache download).
- The projection the UI reads is `spec.packages` (what is asked for) with `status.packages` and
  the condition (what is on disk, and why not yet).

### Host layout (`/nix`, owned by root, world-readable)

```
/nix/store/…                                        the store
/nix/var/nix/db, daemon-socket/socket               Nix's own
/nix/var/nix/gcroots/rustic-profiles                → /nix/var/rustic/profiles  (the one GC root)
/nix/var/rustic/profiles/{ws-id}/                   the DIRECTORY the pod mounts
/nix/var/rustic/profiles/{ws-id}/current            → /nix/store/…-ws-{id}-env
/nix/var/rustic/profiles/{ws-id}/current.building   the in-flight build, renamed over `current`
```

The profile is a directory, not a bare link, because the kubelet resolves a `subPath` **once** at
container start: if the mount were the link, a swap would never reach a running pod. The mount is
the directory and the swap happens one level below it, inside what is already mounted.

The build runs `--no-link --print-out-paths` and the agent makes the `current.building` symlink
itself: `nix build -o` registers an auto-root pointing at the `.building` *path*, which the
publish rename orphans — the live profile would be collectable. Rooting is instead one indirect
root, `/nix/var/nix/gcroots/rustic-profiles → /nix/var/rustic/profiles`, created at agent boot, so
every profile under it is a root for as long as it exists. Deleting the workspace removes the
whole profile directory (the Volume finalizer already runs on the owning node); the next GC frees
whatever nothing else references.

## Reconcile

In `reconcile_workspace`, after `resolve_volume` returns `Ready` and before the pod is ensured.
This runs on **every** pass, which is what "install missing packages at startup" means here:
a restore, a clone, a move to another node, or an agent restart all arrive at step 1 with a
spec whose hash does not match the profile on this node (or no profile at all), and the profile
is rebuilt before the pod starts.

1. Validate `spec.packages` with the grammar above. Invalid → `PackagesReady=False/BuildFailed`
   naming the entry, keep the old profile, `await_change` (only a spec edit can fix it, and that
   is an event).
2. `hash = packages::hash(pin, &spec.packages)`. If `status.packages.observedHash == hash` and
   the `current` link exists → skip to 6.
3. Empty list → the empty `buildEnv` (still built: an empty profile is a valid, mountable
   directory, and "no packages" must not be a special case in the pod spec).
4. Realise, on a blocking thread (`nix` blocks; the pattern is the btrfs work's):
   ```
   nix build --impure \
     --expr 'let pkgs = import (builtins.getFlake "<pin>") { }; in pkgs.buildEnv {
               name = "ws-<id>-env"; paths = [ pkgs.nodejs_20 pkgs.go … ]; }' \
     --no-link --print-out-paths
   ```
   The store path it prints is symlinked to `<id>/current.building` by the agent (not by
   `nix -o`, whose auto-root the publish rename orphans).
   Rendered by `packages::expression`, never by string-formatting user input into a shell: the
   attribute names are validated, and they are passed as a Nix list literal, not interpolated
   into a command line. `nix` is exec'd with an argv, not a shell. Bounded by a deadline
   (`WS_NIX_TIMEOUT`, default 20 min) — a stuck substituter must not hold the reconciler.
5. On success: `rename(<id>.building, <id>)`; write `status.packages` and
   `PackagesReady=True/Built`. On failure: leave the previous profile (if any) in place, write
   `PackagesReady=False/BuildFailed` with the last 20 lines of stderr as the message, and
   requeue at `clamp(now − the condition's lastTransitionTime, 60 s, 3600 s)` — a misspelled
   attribute never becomes buildable on its own, and the fix is a spec edit, which is an event; the pod is still applied with the previous profile if one exists,
   otherwise the workspace waits (a pod whose PATH points at a missing profile is worse than a
   `Creating` workspace with a clear condition). The condition text is what the UI shows — an
   attribute that does not exist reads `error: attribute 'nodejs_99' missing` verbatim.
6. Ensure `nix_pv(id)` and `nix_claim(ns, id)` alongside the live PV/PVC; apply the pod.

While a build runs the pass returns `requeue(TICK)` with `PackagesReady=False/Building` — the
wake-on-finish channel the snapshot reconciler uses is reused, so completion is an event, not a
tick. A spec change is a generation change, which is an event: no polling is involved.

`Resolved::Wait` for the volume stays first: no profile is built for a workspace whose disk does
not exist yet, so a placement failure does not cost a build.

**In-place restore** already scales the pod down before the disk is swapped (`restore_gate`);
the pass that scales it back up goes through steps 1–6. Same for clone (first pass on the new
object — its spec was copied) and move (first pass on the new node).

## Pod

Two additional mounts on the workspace container, both `readOnly: true`, from one PVC
`nix-{id}`:

| subPath (under host `/nix`) | mountPath | why |
|---|---|---|
| `store` | `/nix/store` | profile symlinks are absolute into the store |
| `var/rustic/profiles/{id}` | `/nix/profile` | the workspace's own profile DIRECTORY only — not other workspaces', not the daemon socket |

Environment: `PATH=/nix/profile/current/bin:$PATH` (prepended via the container's `env`; the
image's own `PATH` is unknown, so the entrypoint's `PATH` is extended by `sh -c` only when the
image has no `PATH` env — see `packages::path_env`, tested). Also
`NIX_PROFILE=/nix/profile/current` for tools that want to know, and `MANPATH`/`XDG_DATA_DIRS`
extended the same way for man pages and completions. Every one of them points at `current`, one
level below the mount — that indirection is what a live swap travels through.

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
`WS_NIX_GC_LOW`; the profiles tree is the only root, so GC is always safe. Per-node, not global
— a store is a node's cache, the spec is the truth.

## Moves, clones, restores

- **Move / new node:** `observedHash` matches but the profile is missing → rebuild from the
  spec. Substitutes make this a download, not a compile. `status.packages` is per-object, so a
  workspace on node B never trusts what node A reported.
- **Clone:** `spec.packages` is copied (it is spec). The clone's first pass builds its own profile.
- **Restore / snapshot:** packages are not on the subvolume and are not in the snapshot. A restore
  keeps the current `spec.packages`. (Provenance could carry `packages` for an environment-style
  "restore into a new workspace" later; out of scope.)
- **Delete:** the Workspace finalizer removes the whole profile directory. The nix PV/PVC are
  children by ownerReference, gone with the object.

## API and web

- `POST /v1/workspaces` accepts `packages: string[]` (default `[]`); `PATCH /v1/workspaces/{id}`
  with `{ "packages": [...] }` replaces the list. Both validate with the grammar (422 naming the
  entry) and write spec only; nothing about the node is consulted.
- `GET /v1/workspaces/{id}` and the list project `packages` (spec) and `packagesStatus`
  (`ready`, `reason`, `message`) from the condition.
- Web: the create-workspace dialog gets a **Packages** chip input (free text, one attribute per
  chip, validated client-side with the same regex; a hint links to search.nixos.org). A
  **Packages** section on the workspace's row/page with the same input and "Apply" → PATCH; the
  row shows `installing packages…` / `packages: BuildFailed` with the condition message on
  hover — a misspelled attribute must be visible without opening the pod.

No package search in v1: the attribute list is ~100k entries and changes with the pin;
search.nixos.org is the reference. (ponytail: a cached `nix search --json` per agent is the
upgrade if free text proves error-prone.)

## Security

- Pods: no daemon socket, no writable store, no hostPath, PSA `baseline` unchanged. A workspace
  can read `/nix/store` and its own profile; it cannot see the profile directory of another.
- Agent: the only Nix client. Attribute names are validated at the API **and** again by the
  reconciler before rendering (an object can be written by kubectl); the expression is passed as one argv element; `nix` is
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
| Daemon down | `nix store ping` before every build: `NoNix` with its message, no build attempted; pods with a profile keep running; new workspaces wait and requeue. |
| Agent restarts mid-build | a `current.building` link may exist; the next pass rebuilds (idempotent — Nix's store is content-addressed, the rebuild is a cache hit); `.building` is replaced. |
| `/nix` full | daemon's `min-free` triggers GC; if still full, `BuildFailed` with the disk message. |
| Workspace moved / cloned | rebuild on the node from the spec (see above). |
| `spec.packages` written past the API with a bad entry | `BuildFailed` naming the entry; previous profile stays; fixed by the next spec edit. |

## Testing

- `packages.rs` unit tests: regex accept/reject table (including `$(…)`, quotes, spaces),
  list validation (count, duplicates), expression rendering is byte-exact for a fixed input,
  hash is order-independent and pin-sensitive, `path_env` for images with and without `PATH`.
  `nix.rs`: `valid_pin` accepts only `github:NixOS/nixpkgs/<40-hex>` — the agent refuses to start
  otherwise, since a branch ref makes one hash mean different bits on different days.
- `k8s.rs` tests: the workspace pod has the two read-only mounts and no hostPath (extends the
  existing PSA test); `nix_pv` is read-only with node affinity.
- Agent reconcile tests (mocked API server + a fake `nix` runner): build-then-apply ordering,
  skip on matching hash, `BuildFailed` keeps the pod on the old profile, missing profile rebuilds,
  a spec edit that lands mid-build is rebuilt rather than published under the new hash, a dead
  daemon is `NoNix` and never a build, and a stop keeps the `PackagesReady` condition.
- API tests: create/PATCH validate the list (422 names the entry); the doc projects spec +
  condition.
- `tests/ws_e2e.sh`: a workspace created with `packages: ["hello"]`; `kubectl exec … hello`
  prints "Hello, world!"; PATCH to `["hello", "jq"]`; without a pod restart `jq --version`
  works; PATCH to `["jq"]`; `hello` is gone; clone the workspace; `jq` works in the clone with
  no other action.

## Rollout

1. CRD: add `spec.packages` (default `[]`) and `status.packages` — additive, apply first.
   Existing workspaces have an empty list → an empty profile.
2. Agent DaemonSet: add the `nix-daemon` container, the `/nix` hostPath, the nix.conf ConfigMap,
   `WS_NIXPKGS`; agent image gains the `nix` client. Roll. Existing workspaces: `packages` empty →
   an empty profile is built on their next pass; their pods are NOT restarted for it (the mounts
   are added on the next pod apply, which happens on the next spec change or move). ponytail: a
   pod created before the roll has no `/nix` until it is recreated; acceptable for release 1.
3. API (create/PATCH/projection) + web.

## Out of scope (deliberately)

- Imperative `nix` inside the workspace, user flakes, per-team nixpkgs pins, private binary
  caches, non-nixpkgs sources, a `kloudlite.yaml` in the repo that seeds the spec. Each is
  additive on top of this design.
- Packages for environment services: services are images; that is what images are for.
