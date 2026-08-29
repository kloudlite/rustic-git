# Repo cleanup plan — dead code, redundant code, unwanted files

Date: 2026-08-29. Source: a six-way repo-wide audit (workspaces+agent, api/pulls/git,
storage/core/app/registry+bins, tests, web, deploy/docs/config). Every claim below was
grep-verified by its auditor; the ones marked **verified again** were re-checked by hand
because they are the expensive ones to get wrong.

Total available: **~2,700 lines, 32 files, 15 dependencies.**

Scope is over-engineering only. Correctness, security and performance findings were out of
scope and are not here. Nothing in this plan touches a CLAUDE.md invariant: the routing
middleware's ownership key, the single writer per SlateDB database, "only two things delete a
blob", verbatim manifest bytes, `Digest::parse` as the only path segment → key, labels as views
of `spec.owner`, the events stream never being the record.

---

## Wave 1 — Files that should not be in the repo

No code reads any of these. Land as one commit.

- [ ] `.kube/cache/**` — 28 committed kubectl discovery-cache files, 144 KB, regenerated on
      every `kubectl` call. Not covered by `.gitignore` (`git check-ignore` returns 1).
      `git rm -r --cached .kube` and add `/.kube/` to `.gitignore`. **verified again**
- [ ] `docs/superpowers/poc/wssnap/` — a standalone POC crate (Cargo.toml + 706-line `main.rs`
      + 86-line `suite.sh`) superseded by `crates/workspaces/src/engine/`. The two prose
      mentions elsewhere name the *VM*, not this directory.
- [ ] `docs/superpowers/audit-2026-08-25-raw.md` — 510 lines, self-described "verbatim and
      unedited" inputs to `audit-2026-08-25.md`, itself superseded by `audits/2026-08-29/`.
- [ ] `tests/throughput.rs` — 218 lines, three `#[ignore]`d benchmarks with **zero**
      assertions (`grep -c assert` = 0); nothing in `.github/` or `deploy/` invokes them. The
      numbers live in `docs/perf-bench-2026-08-24.md`. **verified again**
- [ ] `.cargo/audit.toml` — its lone RUSTSEC-2023-0071 ignore is duplicated verbatim in
      `deny.toml`, and both tools run in `image.yml`.
- [ ] `web/apps/web/README.md` — 36 lines of `create-next-app` boilerplate about Geist and
      Vercel deploys, describing files that no longer exist.
- [ ] `web/apps/web/public/{next,vercel,file,globe,window}.svg` — create-next-app leftovers,
      zero references in `src`.
- [ ] `web/apps/web/public/brand/*.svg` (6 files) — `logo.tsx` inlines its own SVG; no
      `/brand/` path appears in any `.tsx`/`.ts`/`.css`.
- [ ] `deploy/RECOVERY.md:359-386` — the "Migrating from the named leader (one-time)" section.
      The StatefulSet it rolls off no longer exists in any manifest and the migration ran on
      2026-08-29.

## Wave 2 — Dead code with zero callers

Each verified by grep across the whole repo including `tests/` and `bins/`.

- [ ] **The draining protocol, ~90 lines across five files.** `OwnershipStore::draining()` has
      zero readers — only `set_draining` writes. It fed the name-based leader's `least_loaded`,
      which was deleted with leader election. Cut `DRAIN_PREFIX`, `set_draining`, `draining()`,
      `App::announce_draining`, the `own_draining` handler and its route, the `| "draining"`
      arm, and both `main.rs` call sites.
      [`crates/storage/src/ownership/mod.rs:91,415-458`, `crates/app/src/lib.rs:631-644,653`,
      `bins/server/src/router/route.rs:119-135`, `router/mod.rs:8,62`, `main.rs:99-104,152-157`]
      **verified again**
- [ ] `migrate_ws_to_vol` + its two tests, 68 lines — a `{pool}/ws` → `{pool}/vol` rename with
      **no production caller at all** (the audit thought it ran every boot; it does not run).
      [`crates/workspaces/src/engine/pool.rs:137-182`] **verified again**
- [ ] `pulls::{set_state, finish_merge, clear_merge}` — 45 lines, zero callers anywhere; merge
      outcomes land through the peer-only outcome route. [`crates/pulls/src/pulls/jobs.rs:172-217`]
      **verified again**
- [ ] `pulls::claim_merge` — 39 lines. Production claims by number only
      (`claim_merge_number`); the scanner's only callers are `tests/pulls.rs:673,693`.
      **verified again**
- [ ] `Directory::all_repos` + the `repos: Collection<Repo>` field, its `db.collection("repos")`
      init and the `Repo` struct — 37 lines, zero call sites; only its own doc comments mention
      the backfill tool it claims to serve. [`crates/pulls/src/directory/mod.rs`] **verified again**
- [ ] `PushOut`'s `sha`/`raw`/`compressed`/`layers`/`elapsed` fields and `PullOut` entirely, 25
      lines — production reads only `.layer`. [`crates/workspaces/src/engine/ops.rs:85-100`]
- [ ] `StoreErr::CasFailed` and `::Conflict` — unconstructible (no `if_match_etag` survives in
      `cosmos.rs`; both `create_*` paths handle 409 before `map_err`), plus `query_items`, a
      generic helper with one caller and a fixed `"SELECT * FROM c"`. 14 lines.
- [ ] `Engine::push` and `Engine::clone_local` — one-line wrappers over `push_env`/
      `clone_local_ids` with zero production callers; tests call the id-taking twins instead.
- [ ] `blob::get_bytes` (5 test-only call sites → move into the test file) and `pull_raw`
      (its own doc says "nothing in production calls it"; make `pull_core` `pub(crate)`).
- [ ] `config::object_store()` — zero callers; everything uses `object_store_views()` or
      `ownership.object_store()`. [`crates/storage/src/config.rs:108-111`]
- [ ] `kube_test::conflict()` — zero callers; the conflict-adopt tests build the 409 inline.
- [ ] `WsState::Cloning` / `EnvState::Cloning` — never constructed (`crd::Phase` has no
      `Cloning`); also removes the dead `state === "cloning"` branches in four web components.
- [ ] `crd::FIELD_MANAGER` (no reader — everything uses `AGENT_FIELD_MANAGER` or a literal),
      `binding::WAIT` (zero callers; both reconcilers use `TICK`), `Config::hostname` (set from
      `HOSTNAME`, never read), `Workspace.live_state` (hardcoded `Value::Null` by its only
      production constructor), the duplicate `nix_claim_name`. **`FIELD_MANAGER` verified again**
- [ ] `orSignIn` (web) — exported with six lines of comment, zero call sites; all six users
      call `listOrSignIn`. Plus the never-imported `SelectScrollUp/DownButton` re-exports.
- [ ] Four doc comments left on the wrong item by earlier refactors, and one orphaned
      `GET /v1/repos?owner=X` doc stranded above `feed_get`.
      [`bins/agent/src/controller.rs:215,677,913`, `lib.rs:558`, `crates/api/src/feed.rs:3-5`]

## Wave 3 — Unused dependencies (15)

- [ ] `rustls` in `bins/{server,api,worker}` — none of the three names it;
      `install_crypto_provider` lives in `crates/storage/src/config.rs`. Keep the workspace pin.
- [ ] Declared-but-never-referenced member deps: `serde_json` (crates/app), `futures`
      (crates/git), `serde` (crates/gitbase), `reqwest` + `chrono` (crates/pulls),
      `kube-runtime` + `reqwest` (bins/agent), `axum` (bins/api), `tracing-subscriber`
      (bins/gateway), `chrono` (bins/server), `redis` (bins/worker).
- [ ] `base64` in `bins/agent` is test-only → `[dev-dependencies]`.
- [ ] `AWS_PROFILE=do` in `.env.example` — no Rust code reads it and `object_store` 0.14.1
      contains no `AWS_PROFILE` string.

Verify each with a build, not just a grep: a dep can be reached through a macro.

## Wave 4 — Mechanical dedup (same behaviour, fewer lines)

- [ ] The four NetworkPolicy builders, **−100 lines** — 186 lines of nested `Some(vec![…])`
      typed structs for four *static* specs. Keep `policy()`, feed it
      `serde_json::from_value(json!({…}))`; only `ns`/`owner` are interpolated.
      [`crates/workspaces/src/k8s.rs:1159-1345`]
- [ ] `claim_workspace` / `claim_environment` are the same 42-line function twice — identical
      attempt loop, `decide`/`bound_elsewhere`/`replace_status`/`ensure_binding`/409 arms; only
      the `Api<K>`, `Phase` and log field differ. One generic `claim<K>`. **−40**
- [ ] Three hand-written status comparators (`status_eq` plus the inline field-by-field guards
      in `write_ws_status`/`write_env_status`) all do what `settled_status_eq` already does
      generically. **−35**
- [ ] Three copies of the `read_dir({pool}/vol)` + `is_dir` + `lineage(id)` walk; `cleanup_local`
      scans the pool twice for two projections of one map. Call `read_lineages` once. **−30**
- [ ] Five hand-inlined copies of "status + `read_bounded` or log-and-502" → `relay(r).await`,
      which is exactly that function. **−25** [images.rs ×2, repos.rs, browse.rs, signatures.rs]
- [ ] `Upstream::delete_volume` / `delete_snapshot` — the same 20-line reqwest DELETE twice →
      `delete_ok(as_owner, path)`, mirroring the existing `get_json`. **−25**
- [ ] Web: three near-identical `error.tsx` (100 lines of identical eyebrow/title/digest/retry
      markup) → one `ErrorPage({title, body, className})` + three ~8-line exports. **−100**
- [ ] Web: the dead `--sidebar-*` and `--chart-1..5` tokens (13 aliases + 39 definitions, zero
      utilities use them) and the unused `@theme` layout tokens. Keep `max-w-auth`.
- [ ] Web: `FastRefresh` is `AutoRefresh` with a different default and no visibilitychange
      listener — delete it; its three call sites become `<AutoRefresh intervalMs={2_000} />`.
- [ ] Tests: the duplicate `pack_of` helper (move `pack_cap.rs`'s copy into `tests/common`),
      the 7× three-line store preamble in `auth.rs`'s unit tests, `env_obj` re-spelling
      `new_env`'s JSON, the poll-and-promote blocks in `tests/ownership.rs`, `ctx_with_registry`
      and `home_volume()` (one caller each), and the `2xx || 404` assertion that cannot fail.
      **−280 total**
- [ ] Smaller shrinks, ~90 lines: `compress_to_file`'s duplicated drain loop, `stop_workspace`'s
      three identical wait/fail statuses, `ensure_profile`'s four-line failure epilogue ×4, the
      `LabelSelector` literal ×3, `ws_volume`/`env_volume`, `feed_get`'s hand-built peer headers.

## Wave 5 — Platform and stdlib replacements

- [ ] `RealNix::run` hand-rolls a 200 ms `try_wait` poll loop plus two `std::thread` pipe-drain
      threads. `tokio::process::Command` + `kill_on_drop(true)` +
      `timeout(t, child.wait_with_output())` does all three. Keep `process_group(0)` and the
      `libc::kill(-pid)` grandchild reap. **−35** [`bins/agent/src/nix.rs:106-163`]
- [ ] The `HostKeys` trait + `SshKeygen` shellout + `FakeHostKeys` + `Ctx::host_keys` field and
      constructor parameter — a single-implementation trait that exists only because the impl
      shells out. `ssh-key` is already in the lock via `russh`:
      `PrivateKey::random(&mut OsRng, Algorithm::Ed25519)` makes it a pure function and the
      trait disappears. **−50**
- [ ] Two manual fill-to-`CHUNK` loops → `r.by_ref().take(CHUNK).read_to_end(&mut buf)?`; the
      second one deletes outright, since `WriteMultipart::new_with_chunk_size` already
      accumulates short writes. **−18**
- [ ] Hand-rolled `hex()` (a `write!` per byte) → `hex::encode`; the crate is already resolved
      in `Cargo.lock`. **−12** [`crates/core/src/err.rs:40-49`]
- [ ] `kube_test::not_found` hand-writes the `Status` JSON literal →
      `Status::failure("not found", "NotFound").with_code(404)`, the constructor already used
      at `api.rs:2003`. **−14**
- [ ] Web: `pull-commits.tsx` and `commit-meta.ts` each build their own `Intl.DateTimeFormat`
      per commit; export `lib/time.ts`'s pinned `ABSOLUTE` and reuse it, so the list and the
      detail page cannot disagree.
- [ ] Five env knobs read in code but set in no manifest, test or script → `const`:
      `RUSTIC_GIT_BLOB_GRACE_SECS`, `_SLATEDB_BLOCK_CACHE_MB`, `_META_CACHE_MB`,
      `_FLUSH_INTERVAL_MS`, `_MAX_CONCURRENT_RECEIVE`, `_MERGE_CACHE_BYTES`.
- [ ] Deduplicate `scheme`/`user_names`/`GIT_PLACEHOLDER`, which exist in both
      `crates/storage/src/auth.rs` and `crates/core/src/httpx.rs`. The doc's "keep storage
      axum-free" justification is void — storage already depends on core. **−20**
- [ ] `forward::read_bounded` is a pure delegate whose only effect is `Vec<u8>` → `Bytes`, and
      every caller wants the `Vec` anyway. `require_jwt_secret` has one non-test caller four
      lines below it. `with_directory` is a builder for one field with one call site that the
      doc says never changes after startup. **−19**
- [ ] Rename `crates/storage/src/cache/disk.rs` → `streams.rs`. Its own module doc says
      "nothing here touches disk; there is no on-disk cache layer in this codebase" — the name
      is an artifact of a plan's file split.

## Needs a decision before touching — do not batch these

- [ ] **Cross-region layer stores** (`region_stores`, `region_stores_from_env`, `region_triples`,
      the `AZURE_REGION_*` triples). The audit called them dead because no manifest sets one.
      They are **documented operator knobs** in `deploy/RECOVERY.md:270`, `BACKUPS.md:203` and
      `deploy/k3s/README.md:199` for restoring from another region, and the code carries a
      `ponytail:` marker. Deleting them removes a documented recovery path, not just code.
      Decide: keep, or delete along with all three doc references. **−50**
- [ ] **The Deployment → StatefulSet migration block** in `run_environment` — a `get_opt` +
      delete of a legacy `Deployment` per service on every reconcile of every environment,
      forever, plus its own 10 s drain, for a conversion that happened once. Safe to delete once
      you confirm no legacy Deployment survives in any environment namespace; that is a cluster
      check, not a grep. **−25**
- [ ] **`generate_ed25519` shelling out to `ssh-keygen`** (plus `tempfile` purely for a writable
      path). `russh` is already a direct dependency and ships ed25519 keygen and OpenSSH
      encoding — but the in-file comment defends the shell-out as avoiding a second `rand_core`
      in the graph. Check the graph before cutting. **−20**
- [ ] **Web: the whole radius scale.** `--radius: 0rem` makes `--radius-sm..4xl` compute to 0,
      so the seven aliases and 39 `rounded-*` classes are no-ops today. Deleting them is correct
      only if sharp corners are permanent (CLAUDE.md says `--radius: 0` — sharp corners
      everywhere). If a future theme might round corners, the scale is the knob that does it.
- [ ] **Web: the CI Triggers and Issues placeholder routes** + the `NotYet` component they exist
      for + the `ci` entry in `sections()`. Delete only if those tabs are not shipping soon —
      this is a product call, not a code one.
- [ ] **Root package + `default-members`.** `[workspace].default-members` repeats the 15-entry
      `members` list verbatim; it can go only if the root `rustic-git-tests` package and its
      one-line `src/lib.rs` stub go too, and `tests/` moves under a real crate. Bigger change
      than its payoff unless the root package is being touched anyway.

---

## Order and verification

Waves 1–3 are deletions with no behaviour to preserve: land them first, one commit per wave,
`cargo test --workspace --locked` and `cargo clippy --workspace --all-targets --locked --
-D warnings` after each. Wave 3 needs a real build, not a grep — a dependency can be reached
through a macro.

Waves 4–5 change code that runs. The existing suites are the oracle: `bins/agent/tests/
reconcile.rs` for the reconciler dedup, `tests/routing.rs` and `tests/ownership.rs` for the
storage and app cuts, `cargo test --test registry_blobs --test registry_http` for the blob
paths, `bun run test && bunx tsc --noEmit -p apps/web/tsconfig.json` for the web items. No
assertion should need editing; if one does, the change was not pure motion — stop and say so.

The four NetworkPolicy builders and the `claim_workspace`/`claim_environment` merge are the two
items worth their own commit and their own review pass. Everything else in waves 4–5 can be
batched by area.
