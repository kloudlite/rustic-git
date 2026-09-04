# Review Fixes — Index and Execution Order

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement each sub-plan task-by-task.

**Goal:** Fix every finding in `docs/code-review-2026-08-23.md` except §7 (missing features).

**Architecture:** Five independent sub-plans, one per subsystem, each producing green `cargo test` / `bun run lint` + `tsc` on its own. Run them in the order below; the cross-plan dependencies are the only coupling.

**Spec:** `docs/code-review-2026-08-23.md`

## Sub-plans (104 tasks total)

| Order | Plan | Tasks | Why this position |
|---|---|---|---|
| 1 | `2026-08-23-review-fixes-registry.md` | 13 | Contains the Critical (worker fences owners). Defines `pub(crate) fn hex` in `src/lib.rs`. |
| 2 | `2026-08-23-review-fixes-http-api.md` | 25 | Defines `App::open_repo_after_fence` (used by ssh/proxy). Signature-verify fix + passkey peer-only. |
| 3 | `2026-08-23-review-fixes-git-core.md` | 24 | Uses `hex` from plan 1. Exposes blocking-safe `objects.rs`/`refs.rs` fns that plan 2's merge task wraps in `spawn_blocking`. |
| 4 | `2026-08-23-review-fixes-web.md` | 22 | Contains the other Critical (passkey login). Independent of Rust plans. Can run in parallel with 1–3. |
| 5 | `2026-08-23-review-fixes-ops.md` | 20 | Last: the CI gate (`clippy --lib -D warnings` + `cargo test` + audit) should land after the code it gates. Non-root containers need the chown/fsGroup steps inside it. |

## Cross-plan dependencies

- `hex` helper: defined in registry plan; git-core plan's `gpg.rs` task depends on it (the task includes the one-line definition as a fallback).
- `App::open_repo_after_fence`: defined in http-api plan; used there for `ssh.rs` and `proxy.rs`.
- Blocking odb helpers: git-core plan exposes them; http-api plan's `merge.rs` task calls them under `spawn_blocking`. If executing http-api first, its merge task wraps the existing sync fns directly — either order compiles.
- Task 3 of the registry plan (manifest PUT checks blob existence) changes test fixtures; the plan lists every site. Run its full suite before starting plan 2.
- `typ: "session"` in JWT (http-api plan) invalidates live sessions once — deploy note in that task's commit body.

## Deliberate deviations from the spec (decided during planning)

- **Registry:** upload-session row is deleted outright (staging object size *is* `have`), which also closes the never-PATCHed leak and three phantom-image sites. `refresh_blob_mtime` removed, not fixed — manifest-PUT `head()` makes it unnecessary. Streaming uses `WriteMultipart` per request (memory = 5 parts), no persisted part list (S3 5 MiB part floor).
- **Git core:** `TreeAdditionsComparedToAncestor` only on the non-shallow commit pass; found and fixed an extra bug (`--filter=blob:none` still sends all blobs). Stale-pack prune has a 1h mtime guard (`ponytail:`).
- **HTTP/api:** negative token cache lives in `auth::Store::lookup`; fingerprint backfill is a startup pass in `Directory::connect`. Mongo-dependent negative paths are covered via extracted pure helpers (no `Directory` fixture exists).
- **Web:** `pathHref` in `lib/utils.ts` (not `browse.ts`, which is `server-only`). SSO branch, `dev-auth.ts`, `ThemePicker`, mock `team-*`/`declared-list` deleted rather than hidden.
- **Ops:** `cargo fmt --check` NOT gated (938 hunks today — repo-wide reformat is the owner's call). `kloudlite-git-lb` http port replaced by a ClusterIP `kloudlite-git-http` (both Ingresses back onto it). `compat-matrix.sh` kept (covers things `http_e2e.rs` doesn't). Routing-test sleeps replaced by a per-`App` skewable clock, not `MockSystemClock` (wrong layer). `browse_http.rs:193` sleep left (not a flake).
