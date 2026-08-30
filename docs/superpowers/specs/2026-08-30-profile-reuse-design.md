# Reusing a package profile across workspaces — design

Date: 2026-08-30. Status: draft for review.

## Problem

Creating a workspace or a clone waits on a Nix profile build even when an identical profile already
exists on the node. Measured on the live cluster:

| | |
|---|---|
| Clone's btrfs snapshot (Workspace created → Volume `Ready`) | **0 s** |
| Profile build, first after the agent started | **28 s** |
| Profile build, second | **1 s** |
| Pod start → `Ready` | ~5 s |

The 28 s is not the package. `hello` was already in the store and nothing was downloaded; running
the agent's own expression on the node took 28.065 s cold, 1.372 s warm, 0.324 s after that. It is
the cost of fetching and evaluating nixpkgs, and it is paid again after every agent restart because
the eval cache lives at `/root/.cache/nix` (396 MB) on the container's overlay filesystem.

Two things make it worse than it needs to be.

**The profile is keyed to the workspace, not to its inputs.** `packages::expression` builds

```
pkgs.buildEnv { name = "ws-{id}-env"; paths = [ … ]; }
```

The workspace id is in the derivation name, so two workspaces with the same pin and the same
package list produce different derivations and different store paths. A clone cannot reuse its
source's profile even though every input matches.

**The "already built" check is per workspace.** `ensure_profile` skips the build when
`status.packages.observedHash` equals `packages::hash(&pin, &all)` and the profile exists on disk.
A new workspace has no status, so it always misses — even on a node holding an identical profile.

The hash that would answer the question already exists and is already computed on every reconcile.
It is simply not consulted anywhere outside the one workspace that produced it.

## What must not change

Reconciling a profile is how the agent guarantees the installed set matches `spec.packages`. That
guarantee is preserved exactly here, because it is a pure function of `(nixpkgs pin, base packages,
spec.packages)` — the three inputs `packages::hash` already covers. Comparing that key is the same
check as re-deriving the answer, minus the derivation. Only a change to one of those inputs may
skip the cache, and a change to any of them changes the key.

## Design

### 1. A node-level index, keyed by the hash that already exists

Alongside `{profiles_dir}/{id}/current`, the agent maintains

```
/nix/var/rustic/profiles/by-inputs/{hash} -> /nix/store/…-env
```

`ensure_profile` gains one step before it decides to build: if that link resolves and its target
still exists, publish `{id}/current` pointing at the same store path and record the status as
built. No nix invocation, no evaluation. On a miss it builds as it does today, then writes both the
per-workspace link and the index entry.

The per-workspace `{id}/current` link stays exactly as it is. The pod mounts `{profiles_dir}/{id}`
as a subPath and the live swap happens one level below it; nothing about that changes.

`{profiles_dir}` is on the host under `/nix` (the `seed-store` init container creates it) and is
already GC-rooted through `gcroots/rustic-profiles`, so an index entry is durable across agent
restarts and its store path cannot be collected while the link exists.

### 2. Drop the workspace id from the derivation name

`name = "ws-{id}-env"` becomes a name derived from the inputs, so identical inputs produce one
store path shared by every workspace and clone. With this, a miss on the index that nonetheless
matches an existing derivation is a store cache hit rather than a rebuild — the evaluation still
runs, but nothing is realised.

This changes the store path a given set of inputs derives to, but it does NOT re-derive anything
that already exists. `packages::hash` covers `(pin, base, spec.packages)` and not the derivation
name, so an existing workspace's `observedHash` still matches, its `{id}/current` still resolves,
and `ensure_profile` takes its per-workspace early return: it keeps its old `ws-{id}-env` profile
for as long as it lives, is never rebuilt, and never lands in the index. The sharing therefore arms
for workspaces created after the deploy — the first new workspace per package set on each node
still pays the cold build, and every one after it is an index hit. Nothing is lost by that: the old
paths stay rooted by their own `{id}/current`, and forcing a rebuild would be churn for no gain.

### 3. Move the eval cache off the overlay

Set `XDG_CACHE_HOME=/nix/var/rustic/cache` on the agent container, so nix's evaluation cache lives
on the host `/nix` the agent already mounts rather than in the pod's writable layer. The 28 s then
becomes once per node rather than once per agent restart, and is skipped entirely whenever the
index hits.

Budget for it: 396 MB on the node today.

## What this makes fast

- A clone whose package list matches its source: **no nix work at all** — an index hit.
- A new workspace on a node that has built that set before: same.
- The first ever build of a set on a node: unchanged, minus the repeat evaluation after restarts.

## Pruning

Index entries are GC roots, so they keep their store paths alive. A user who edits their package
list repeatedly leaves an entry per distinct set, each rooting an env.

The janitor gains a sweep, in the shape of the existing ones: remove `by-inputs/{hash}` links older
than a bound that no `{id}/current` resolves to. Keep-biased like its siblings — an unreadable
profiles directory sweeps nothing. The entries are cheap (one symlink each, and the store paths are
shared), so the bound should be generous; this exists so the set cannot grow without limit, not to
reclaim quickly.

## Failure modes

| Failure | Behaviour |
|---|---|
| Index entry points at a store path that no longer exists (a GC raced, or the store was rebuilt) | Treated as a miss: the target's existence is checked, not just the link's. Builds normally and rewrites the entry. |
| Index entry is a dangling symlink or a directory | Same — removed and rebuilt. Never followed blindly. |
| Two reconciles build the same set at once | Both write the same link to the same store path; the write is idempotent. The existing per-workspace single-flight (`ctx.running`, keyed `profile:{uid}`) is unchanged. |
| A spec edit lands mid-build | Unchanged: the existing `started_from != hash` check drops the stale result and rebuilds. |
| `{profiles_dir}` unwritable | The index write fails and is logged; the build still publishes the per-workspace link, so the profile is correct and only the sharing is lost. |
| Nix daemon down | Unchanged — `NoNix`, and a workspace that already has a profile still gets its pod. |
| The eval cache path is missing on a node | Nix recreates it; the first build there is the cold 28 s, as today. |

## Not in scope

Changing what a profile contains, the base package list, the pin, or how `spec.packages` is
validated. Prebuilding profiles for package sets nobody has asked for. Sharing profiles between
nodes (the store is per node; that is what the registry is for elsewhere).

## Tests

- `crates/workspaces` units: `packages::expression` no longer contains the workspace id, and two
  different ids with the same pin and packages produce byte-identical expressions;
  `packages::hash` is unchanged (it already keys on the right three inputs).
- `bins/agent/tests/reconcile.rs`: a workspace whose inputs match an existing index entry reaches
  `PackagesReady` without the fake nix being asked to build; an entry whose target is missing is
  treated as a miss; a build writes both links.
- `bins/agent` janitor tests, in the shape of the existing sweeps: an old unreferenced index entry
  is removed, a referenced one and a young one are kept, an unreadable directory sweeps nothing.
- Measured, on the cluster, after deploying: a clone of a workspace with a non-empty package list
  reaches `Ready` without a 28 s `PackagesReady` gap. That number is the point of the change, so it
  is checked against reality, not only in tests.
