# Package versions: `name@version` in a workspace's packages

Status: draft for review, 2026-09-08. Follows devbox's model (verified against its docs and the
live `search.devbox.sh` API on 2026-09-08).

## Why

`spec.packages` today is a list of nixpkgs attribute names, all taken from the region's one
nixpkgs pin. A person who needs `nodejs` 20 while the pin ships 24, or `python3` 3.11 for a
project that is not ready for 3.13, has no way to say so; the `nodejs_20` attribute exists for
some packages and not for most, and it still means "whatever 20.x the pin has". Devbox solved
this with `name@version` resolved through a version index and locked, and people expect it.

## Decisions taken

| Question | Answer |
| --- | --- |
| Partial versions (`@20`, `@3.11`) | the newest release matching the prefix, at write time |
| When a lock moves | only on an explicit update (`POST /v1/workspaces/{id}/packages/update`), devbox's default; editing other entries never moves an untouched lock |
| Resolver | Jetify's `search.devbox.sh` (Nixhub) first, a mirrored nixpkgs-multiverse index in the object store as fallback; both cached 24 h |
| Binary cache miss | fail fast (`NotCached`), never a source build on a shared node |
| Bare `attr` | unchanged: the region's nixpkgs pin, one evaluation for the whole list |
| Where the lock lives | `spec.locks`, written by the api beside `spec.packages` |

## Design

### 1. Grammar (`crates/workspaces/src/packages.rs`)

An entry is `attr` or `attr@version`.

- `attr`: today's grammar (`validate_attr`), unchanged.
- `version`: `latest`, `N`, `N.N` or `N.N.N`, digits only. Anything else (`^20`, `20.x`,
  `20-rc1`, `>=20`) is refused with the entry named.
- The list's duplicate rule keys on `attr`: `nodejs` and `nodejs@20` in one list is
  `Duplicate("nodejs")`.
- `MAX_PACKAGES` (100) and `MAX_ATTR_LEN` (64) apply to the whole entry.

The grammar is checked twice, as today: by the api before it writes, by the agent before it
renders anything into a Nix expression. The agent never sees an unresolved `@` entry (§3), but it
checks anyway; a CR can be written by any principal with write access.

### 2. Resolution (`bins/api`, at write time)

`spec.locks` is a list of

```yaml
locks:
  - entry: nodejs@20            # the spec entry this lock is for, verbatim
    version: "20.20.2"          # what it resolved to
    attrPath: nodejs_20         # the nixpkgs attribute at that revision
    rev: 389ed85304b281ca7f306cf8a1eb4378651ca44e   # the nixpkgs commit
    storePath: /nix/store/fr6ck…-nodejs-20.20.2      # x86_64-linux output, the thing the agent fetches
    resolvedAt: 2026-09-08T09:12:00Z
    source: nixhub              # nixhub | mirror
```

**When.** On create, clone, restore and every `PATCH` of `packages`, the api resolves each `@`
entry that has no lock or whose string changed; existing locks for unchanged strings are carried
over untouched. `POST /v1/workspaces/{id}/packages/update` re-resolves every `@` entry (exact
`N.N.N` pins included: an exact pin can still move to a newer nixpkgs revision that built the
same version, which is what devbox's `update` does) and writes the new locks; a lock that did not
change writes nothing. Clone and restore copy `locks` from the source snapshot's `spec.state`
unchanged, so a restored workspace builds exactly what was frozen.

**How.** `crates/workspaces/src/packages/resolve.rs` (new): `Resolver { nixhub, mirror, cache }`.

1. Cache: `pkgs/x86_64-linux/{attr}/{version}` in the object store, a JSON `Lock` with
   `resolvedAt`; fresh for 24 h. `@latest` and partial versions are cache keys too (they are what
   the person typed), so two people asking for `nodejs@20` on the same day get the same lock.
2. Nixhub: `GET https://search.devbox.sh/v2/resolve?name={attr}&version={version}`, 10 s
   timeout, the `systems["x86_64-linux"]` entry gives `flake_installable.ref.rev`, `attr_path`,
   `outputs[default].path`. A 404 is "unknown"; a 5xx or timeout is "unavailable".
3. Mirror: `index/pkgs/versions.json` in the object store, the nixpkgs-multiverse
   `index/versions.json` fetched by the admin process on a daily beat (`history::beats`, the tier
   that already runs periodic work and has outbound access). Resolution: newest release whose
   version string starts with the requested prefix. The mirror carries `rev` and `attr_path` but
   no store path; a mirror-resolved lock leaves `storePath` empty and the agent evaluates that
   revision's attribute instead (§3), which is slower and is why Nixhub is first.

Answers, in order: cache hit → Nixhub → mirror. Nixhub "unknown" AND mirror "unknown" →
`422 "nodejs@20.99 is not a version anyone published; nearest: 20.20.2, 20.19.5, 20.18.3"`
(the three closest from whichever index answered). Nixhub unavailable AND no cache AND mirror
absent → `503 "the package index is unavailable; try again"`. The api never writes a guessed
lock and never writes a `@` entry without one.

**Where.** `crates/workspaces/src/api/workspaces.rs` create/patch/clone/restore call
`resolve::lock_all(&resolver, &spec.packages, &prev_locks) -> Result<Vec<Lock>, Refusal>`
before the CR write, the same place `guard_alloc` runs today, so an unresolvable list never
reaches the cluster.

### 3. Build (`bins/agent`)

`packages::hash` covers the region pin, the sorted bare entries, and the sorted lock
`storePath`s (or `rev#attrPath` for a mirror lock without one). Same inputs, same hash, same
store path, same `by-inputs` index hit as today.

`packages::expression(pin, bare, locks)` renders:

```nix
let pkgs = import (builtins.getFlake "<pin>") { };
in pkgs.buildEnv {
  name = "kloudlite-workspace-env";
  paths = [ pkgs.jq pkgs.ripgrep
            (builtins.storePath "/nix/store/fr6ck…-nodejs-20.20.2")
            ((import (builtins.getFlake "github:NixOS/nixpkgs/<rev>") { }).python311) ];
}
```

Before evaluating, the agent runs `nix copy --from https://cache.nixos.org <storePath>` for every
lock that has one (in parallel, bounded by `nix_timeout_secs`). A path the cache does not hold
ends the reconcile with `PackagesReady=False/NotCached` — message
`"nodejs@20.20.2 has no binary in cache.nixos.org; pick a version that does"` — and no source
build is attempted: `nix build` runs with `--option substituters https://cache.nixos.org
--max-jobs 0`, so an uncached derivation fails instead of compiling. A mirror lock (no store
path) evaluates its revision, costing one nixpkgs evaluation per distinct `rev` (~28 s cold,
cached by the daemon after), and is subject to the same `--max-jobs 0` rule.

The store paths are gcrooted by the profile as today (`ensure_gcroot`), so a lock whose binary
later leaves the cache keeps working on every node that already holds it; only a first build on
a new node fails as `NotCached`.

### 4. Status and web

`status.packages` gains `locked: [{entry, version, rev}]`, written with the rest of
`PackagesStatus` on every reconcile from `spec.locks`, so the web shows `nodejs@20 → 20.20.2`
beside each entry. Two new `PackagesReady` reasons: `NotCached` (§3) and `Unresolved` (an `@`
entry with no lock reached the agent; the api never does this, an operator with `kubectl` can).

Web: the package editor accepts the new grammar client-side (same regex as the api, so a typo is
refused before a request), renders the resolved version and the resolver's 422 message verbatim,
and the workspace page gets "Update pinned packages" calling the update route. `SnapshotState`
freezes `locks` with `packages`; restore's defaults include them.

### 5. Failure modes

| Failure | Behaviour |
| --- | --- |
| Nixhub down or slow | cached lock if under 24 h, else the mirror; else 503 at the api, no CR write |
| Mirror stale (daily beat missed) | a newer patch release is missed; never a wrong package |
| Version unknown everywhere | 422 with the three nearest versions; nothing written |
| Binary not in cache.nixos.org | `NotCached` within seconds; no source build; nothing else in the list is affected |
| Lock's binary later dropped from the cache | nodes that built it keep it (gcroot); a new node reports `NotCached` |
| `@` entry with no lock in the CR | `Unresolved`; the api rewrites the lock on the next spec write or update |
| Agent offline | nothing changes; the lock is in the CR and builds on the next reconcile |

### 6. Out of scope

Pins for the base set (`ClusterSettings.basePackages` stays bare attributes on the region pin);
arbitrary flake references; per-package nixpkgs overrides; non-`x86_64-linux` nodes (the
resolver returns per-system paths, and the agent's system is a one-line change when such a node
exists); source builds on request.

## Files

`crates/workspaces/src/packages.rs` (grammar, hash, expression), `crates/workspaces/src/packages/resolve.rs`
(new: `Lock`, `Resolver`, cache, Nixhub client, mirror lookup), `crates/workspaces/src/crd/mod.rs`
(`WorkspaceSpec.locks`, `SnapshotState.locks`, `PackagesStatus.locked`, regenerated `crds.yaml`),
`crates/workspaces/src/api/workspaces.rs` (resolve before write; the update route),
`crates/workspaces/src/history/beats.rs` (mirror refresh), `bins/agent/src/nix.rs` (`nix copy`
substitution, `--max-jobs 0`), `bins/agent/src/controller/workspace.rs` (locks into the hash and
expression, `NotCached`/`Unresolved`), `web/apps/web` (grammar, resolved versions, update action),
`deploy/slo.md` + `crates/workspaces/src/slo/catalogue.rs` (a `ws.packages.pin` step in the hourly
suite: create with `jq@1.7` — a version cache.nixos.org holds — and assert the lock and the binary).
