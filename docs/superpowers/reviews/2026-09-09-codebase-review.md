# Codebase review — Phase 1 findings

**Date:** 2026-09-09 · **Tree:** master `4cda19c8` · **Scope:** `crates/`, `bins/`, `tests/`, `web/apps/web`
**Method:** static sweep (sizes, comment density, `unwrap`/`expect` outside `#[cfg(test)]`, duplicated
helpers, function length, list-walks per request, blocking IO in async files, env reads, secret
logging), `cargo clippy` pedantic+nursery, `npm audit` on the web workspace, and a read of every
trust boundary (`registry::auth::allow`, `api::scope::may_act_on`, `admin::refuse_without_claim`,
the gateway's session verify, the builder gate's secret, the pod-fence VAP). `cargo audit` /
`cargo deny` are appended at the end when the install in the dev pod finishes.

Numbers first: 156k lines (crates 62.7k, bins 50.3k, tests 15.7k, web 27.6k). 30 source files
over 900 lines; largest `crates/workspaces/src/k8s.rs` 3116, `crd/mod.rs` 2063, `app/lib.rs` 1535,
`agent/controller/workspace.rs` 1521, `web/lib/api.ts` 1679, `agent/tests/reconcile.rs` 6557.
Comment density 12–42 % by file. Tests: 1400+ test fns; `bins/api` has none.

## Findings, ranked

Severity is impact × likelihood; cost is the fix's size (S < 1 h, M < 1 day, L multi-day).

| # | Sev | Area | Finding | Evidence | Fix | Cost |
|---|-----|------|---------|----------|-----|------|
| 1 | HIGH | reliability | `panic = "abort"` in the release profile means any `unwrap`/`expect` reached on a request or reconcile path kills the whole pod, not one request. Request-facing crates are mostly clean, but nothing enforces it. | `Cargo.toml:188`; non-test `expect` in `bins/agent/src/controller/volume.rs` (7), `crates/workspaces/src/api` (15 `expect`); no `[workspace.lints]` at all | `clippy::unwrap_used`/`expect_used` = deny for `crates/api`, `crates/registry`, `crates/workspaces::api`, `bins/{server,gateway,builder-gate,api}`; allow per-site with a reason where a poisoned `Mutex` is the only case | M |
| 2 | HIGH | reliability | Nine outbound `reqwest::Client::new()` sites carry no timeout; a hung upstream hangs the caller forever. `crates/api/src/forward.rs` (3) is on the request path; `packages/resolve.rs`, `history/notify.rs`, `mirror_beat.rs` are beats that then never tick again. | `grep reqwest::Client::new()`: `crates/api/src/{forward.rs:153,162,200, repos.rs:941, lib.rs:459}`, `crates/workspaces/src/{history/notify.rs:14, packages/mirror_beat.rs:70, packages/resolve.rs:400,406}` | One `core::http::client(timeout)` builder with connect + total timeouts; every site uses it | S |
| 3 | MED | architecture | A library crate depends on a binary crate: `crates/workspaces` → `bins/server` (`Cargo.toml:56`), and `crates/git` → `crates/app` (`lib.rs:5`). Both invert the layering the guide describes (`crates/*` are libraries; bins compose them) and make `kloudlite-workspaces` unbuildable without the server. | `crates/workspaces/Cargo.toml:56`; `crates/git/src/lib.rs:5` | Move whatever `workspaces` takes from `bins/server` (test helpers?) into `crates/core` or a `crates/testkit`; `git` should take an `App`-shaped trait, not `kloudlite_app::App` | M |
| 4 | MED | performance | Admin pages walk the whole fleet per request: `admin/owners.rs:76-81` lists Quota, Request, Workspace, Environment, Volume, Snapshot cluster-wide on every call; `admin/clusters.rs` 9 list calls; the api holds one reflector in total. Fine at 3 nodes, quadratic at 300. `quota::usage` (4 label-selected lists per `/v1` create) is deliberate per the guide and stays. | `crates/workspaces/src/api/admin/{owners.rs,clusters.rs}`; `grep -c reflector crates/workspaces/src/api` = 1 | A `Store` per CRD kind in the admin process (kube-runtime reflector), admin readers read the store; `/v1` keeps its per-request usage walk | M |
| 5 | MED | best practice | ~20 direct `std::env::var` reads outside `settings`/`main`, against the guide's "never read env for a knob that has a Settings field": `KLOUDLITE_MAX_LAYER`, `KLOUDLITE_UPLOAD_GRACE_SECS`, `KLOUDLITE_EXTERNAL_URL`, ClickHouse URL/user/password, `KLOUDLITE_NIXHUB_URL`, `KLOUDLITE_BINARY_CACHE_URL`, `KLOUDLITE_WEB_URL`, `KLOUDLITE_HYPERDX_URL`, `KLOUDLITE_CACHE_DIR`. Some are boot-only by nature (S3 URL, cache dir); the layer/grace/external-url ones are tunables read on every request. | `grep std::env::var crates bins` | Fold the tunables into `Settings` (central document); leave boot-only ones but read them once in `main` and pass them in | M |
| 6 | MED | maintainability | Twenty functions over 150 lines; the top: `history/alerts.rs::two_metric_ratio` 386, `agent/controller/environment.rs::run_environment` 374, `slo/catalogue.rs::find` 353 (a table), `agent/controller/run.rs::run` 318, `bins/api/main.rs::run` 281, `agent/controller/workspace.rs::ensure_profile` 274, `git/protocol/upload/mod.rs::fetch` 268, `crates/api/lib.rs::serve` 254, `bins/server/main.rs::serve` 245, `router/route.rs::route_inner` 234. | function-length scan | Phase 2 splits; `run_environment`/`ensure_profile`/`route_inner` first — they carry the most branching | L |
| 7 | MED | reliability | Blocking filesystem and process calls inside async files: `bins/agent/src/nix.rs` (35 `std::fs`/`Command` uses), `controller/workspace.rs` (18), `controller/keys.rs` (12), `snapshot.rs` (9). Where these run on the reactor thread they stall every other reconcile on that node for the duration of a `btrfs`/`nix` call. Some are already under `spawn_blocking` (`ensure_shared_home`, `drop_snapshot`); the rest need a pass. | file scan; `spawn_blocking` present in 3 of the 4 | Audit each call site; wrap in `spawn_blocking` or move to `tokio::fs`; a `clippy::disallowed_methods` entry for `std::fs::*` in `bins/agent` keeps it that way | M |
| 8 | MED | tests | `bins/api` has zero tests (the composition root: role switch, directory wiring, resync beats). `crates/git` 8, `crates/registry` 12 unit tests — their coverage lives in `tests/` integration files. The full parallel `cargo test` hangs intermittently in the root package's `api_server`/`browse` binaries (pass alone; three hung binaries found on 2026-09-09), which is why the ship gate has a watchdog. | test counts; `/tmp/ship-test.log` on the dev pod | Find the hang (shared fixed port or a global lock across test binaries — run with `--test-threads=1` per binary to bisect); add a boot test for `bins/api` role selection | M |
| 9 | LOW | security | Web deps: 3 low-severity advisories via `@simplewebauthn/server` ← `@auth/core` ← `next-auth`. | `npm audit --omit=dev` | Bump `next-auth`/`@auth/core` | S |
| 10 | LOW | redundancy | Duplicated helpers: `clip()` ×4 in `bins/slo` (`step.rs` has the JSON-safe one; three stages carry a naive `chars().take(200)`), `env_u64()` ×3 (`storage/pool`, `core/settings`, `workspaces/settings`), `id_of`/`state_is` ×3 across slo stages, `from_env()` ×6 with the same shape. | fn-name scan | One `slo::util` module; one `core::env` helper; delete the copies | S |
| 11 | LOW | style | No `[workspace.lints]`; pedantic+nursery reports 2129 warnings, dominated by doc formatting (`missing backticks` 273, `first paragraph too long` 264, `pub(crate) in private module` 284) and 57 numeric-cast warnings (`u64→i64` 32, `u128→u64` 25) worth a look where they touch quotas or timestamps. | `/tmp/pedantic.txt` | Adopt a small lint set in `[workspace.lints]`: `unwrap_used` (request crates), `cast_possible_truncation`, `disallowed_methods`; leave doc-style lints off | S |
| 12 | LOW | docs | Comment density 15–42 %: design rationale and incident history sit inline next to code, so a reader meets the same paragraph in three files and the module has no front page. | density scan | Phase 2: every split module gets a `//!` header carrying the context; inline comments keep only the line-level "why" | L |
| 13 | INFO | security | Verified sound: registry `allow()` fails closed on directory errors (`unwrap_or(false)` both branches) and only challenges anonymous callers; `may_act_on` is owner/team/superadmin with the superadmin act logged; admin router refuses without the claim before any route; signin has per-IP and per-email limiters; gateway verifies the ssh-session JWT before splicing; builder gate fails closed without its secret; the pod-fence VAP re-checks caps/hostPath/gvisor at admission; the only `dangerouslySetInnerHTML` sites are shiki output and the theme bootstrap script; the passkey cookie is httpOnly+strict. | reads | Two things to confirm in Phase 3: the main web session token's storage (bearer header from where — cookie flags), and constant-time comparison of `KLOUDLITE_BUILDER_SECRET` on `/v1/internal/builders/*` | S |
| 14 | INFO | deps | 735 packages in `Cargo.lock`, 872 crate versions in the tree; `cargo audit`/`cargo deny` results below. | `cargo tree` | Trim after the audit — likely duplicates of `syn`/`hashbrown`/`rustls` majors | S |

## Modularisation plan (Phase 2 targets)

Rule: no source file over ~800 lines, one responsibility per file, a `//!` module doc that
carries the design context. Behaviour-neutral; each slice ships with tests + clippy green and a
fast probe pass before merge.

| File | Lines | Split into |
|------|-------|------------|
| `crates/workspaces/src/k8s.rs` | 3116 | `k8s/{mod,labels,namespace,secrets,workspace_pod,environment,builder,policies,resolv,quantities}.rs` |
| `crates/workspaces/src/crd/mod.rs` | 2063 | `crd/{mod,workspace,environment,volume,snapshot,quota,request,region,settings,conditions}.rs` (already has `names.rs`) |
| `crates/app/src/lib.rs` | 1535 | `app/{mod,election,ownership,directory,may_act}.rs` |
| `bins/agent/src/controller/workspace.rs` | 1521 | `workspace/{mod,home,profile,pod,status}.rs`; `ensure_profile` becomes its own module |
| `bins/agent/src/controller/environment.rs` | 1194 | `environment/{mod,namespace,services,intercept,stop}.rs`; `run_environment` split by phase |
| `crates/workspaces/src/api/workspaces.rs` + `mod.rs` | 1253 + 1104 | `api/workspaces/{create,lifecycle,clone_restore,packages}.rs`; `mod.rs` keeps router + state |
| `crates/pulls/src/directory/mod.rs` | 1547 | `directory/{mod,mongo,memory,signin,superadmins}.rs` |
| `crates/workspaces/src/packages/resolve.rs` | 1183 | `resolve/{mod,nixhub,mirror,cache_check}.rs` |
| `bins/slo/src/stages/experience_gaps.rs`, `weekly_gaps.rs` | 1418, 1285 | by probe family; shared `slo::util` |
| `web/apps/web/src/lib/api.ts` | 1679 | `lib/api/{client,workspaces,environments,admin,keys}.ts` |
| `bins/agent/tests/reconcile.rs` | 6557 | `tests/reconcile/{workspace,environment,volume,claim,keys}.rs` with one `common.rs` |

Order: `k8s.rs` → `crd/mod.rs` → agent controllers → `app/lib.rs` → api → directory → slo →
web → tests. `CLAUDE.md` gains a module map and the house-style line becomes "context in module
docs, why at the line".

## Dependency audit

`cargo audit`: **0 vulnerabilities**; 3 unmaintained-crate warnings — `bincode` (RUSTSEC-2025-0141),
`paste` (RUSTSEC-2024-0436), `rustls-pemfile` (RUSTSEC-2025-0134) — all transitive, none on a
request path; replace when their parents move. `cargo deny check advisories licenses bans sources`:
all four **ok**, with **80 duplicate-version warnings** (the same crate at two or more majors in the
tree) — the trimming candidate from finding 14. Neither tool is in CI; adding `cargo deny check` to
`image.yml`'s test job is a one-line follow-up.
