# Workspace restructure — design

**Goal:** cut the single `rustic_git` library into a Cargo workspace so each of the three
binaries links only what it uses, incremental builds rebuild one crate instead of everything,
and the eight files over 700 lines become focused modules.

**Non-goals:** independent versioning/publishing; image-per-component (one image, three
binaries, unchanged deploy yamls); splitting files under 700 lines; any behavior change at all.

**Migration strategy (approved: A):** bottom-up, crate-at-a-time. The old `rustic_git` lib
shrinks step by step and re-exports every moved module (`pub use …`) so the binaries and all
integration tests compile green after every extraction. The facade is deleted in the final
step, when consumers' imports are rewritten once. Every step ships alone; `cargo test` and
`clippy -D warnings` gate each one.

## Measured dependency map (2026-08-24, `grep crate::` per module)

Cycles and cross-cutting edges that shape the cut — each with its resolution:

| edge | resolution |
|---|---|
| `events ↔ cache` | cohabit in `storage` |
| `pulls ↔ directory` | cohabit in `pulls` (directory moves there; `api` depends on `pulls`) |
| `proxy ↔ ssh`, `proxy → protocol` | **split `proxy`**: the peer HTTP client half (peer headers/auth, no russh) becomes `core::peer`, used by worker/api; the fleet-forwarding half stays server-side in `git` |
| `protocol → http`, `registry → http` | the shared bits (body limits, error envelope helpers) extract to `core::httpx`; the router itself moves to the server binary crate |
| `auth → store` | `auth` lives in `storage`, not core |
| `config → store, cache` | `config` lives in `storage` |
| `gc → protocol` | `gc` lives in `git` |
| `pulls → browse` (gix merge-base) | **split `pulls`** along its planned file split: `model`/`jobs` (no gix — what the worker links) vs `check` (gix, server-only, feature-gated or a separate module the worker never imports; final seam decided in the plan) |

## Target layout

```
Cargo.toml                # workspace root; [workspace.dependencies] pins every shared version
crates/
  core/       err, jwt, hex, pktline, peer (client half of proxy), httpx
  storage/    store, pool, ownership, index, cache, events, auth, config, objects*, refs*
  git/        protocol/, browse, gpg, ssh, gc, proxy (server half)
  registry/   registry/* (OCI: blobs, manifests, uploads, gc, auth, routes, store)
  pulls/      pulls (model/check/jobs), merge_worker, directory
  api/        api/* (repos, feed, browse proxy, teams/users handlers)
bins/
  server/     ex-main.rs split: boot, lanes, listeners, router (ex-http.rs) — links everything
  api/        core + storage(read) + api
  worker/     core + storage + registry + pulls  — no russh, no gpg, no gix-pack
```
*`objects`/`refs` sit on `store` and are needed by both `git` and `pulls`; they go in `storage`
so `pulls` need not link `git`. If extraction shows they drag gix types into `storage`'s public
surface, the plan may move them to a small `gitbase` crate between `storage` and `git` — the
decision point is named in the plan, not improvised.

Binary names, image contents, deploy yamls, env vars: unchanged. `cargo build --release
--locked` at the root builds all three binaries with the same names.

## File splits (only these eight; cut lines decided from the code during extraction)

| file | lines | becomes |
|---|---|---|
| `src/http.rs` | 1040 | `bins/server/src/router/{route,git,limits}.rs` |
| `src/protocol/upload.rs` | 974 | `git/src/protocol/upload/{refs,walk,pack}.rs` |
| `src/lib.rs` | 964 | dissolves: App/lanes → `bins/server`, helpers → owning crates |
| `src/main.rs` | 922 | `bins/server/src/{boot,lanes,listeners}.rs` |
| `src/pool.rs` | 891 | `storage/src/pool/{mod,lease,evict}.rs` |
| `src/directory.rs` | 775 | `pulls/src/directory/{mod,teams}.rs` |
| `src/pulls.rs` | 773 | `pulls/src/{model,check,jobs}.rs` |
| `src/cache.rs` | 766 | `storage/src/cache/{mod,disk}.rs` |

## Tests

Unit tests move with their modules. Integration tests under `tests/` stay at the workspace
root as a `tests` member (or attach to `bins/server`, decided in the plan) since most exercise
the composed server; per-crate tests migrate only where they clearly test one crate.
`every_browse_route_is_routable` moves with the router and must stay green throughout.

## Expected wins

- `rustic-git-api` binary: drops gix*, russh, pgp/gpg, flate2, imara-diff, registry code.
- `rustic-git-worker`: drops russh, pgp/gpg, gix-pack/traverse (keeps object-store/slatedb).
- Touching `registry/` rebuilds `registry` + two bins, not the world; touching web of course
  unchanged.
- Eight focused module trees instead of monoliths.

## Risks

- Hidden cycles beyond the measured map (module-level grep misses item-level knots. e.g. a
  `store` method returning a `protocol` type). Resolution defaults: move the item to the lower
  crate or introduce a narrow trait in `core`; upgrade to a design conversation if a seam
  refuses both.
- The facade period means `rustic_git` temporarily depends on every extracted crate — build
  times get *worse* until the final step removes it. Accepted; each extraction still lands green.
- `pulls` model/check seam is the one place behavior could shift if the split moves code across
  an `await` boundary; the worker_merges suite (37 tests) is the guard.
