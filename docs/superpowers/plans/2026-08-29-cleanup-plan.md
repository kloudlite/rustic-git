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

- [x] `.kube/cache/**` — 28 committed kubectl discovery-cache files, 144 KB, regenerated on
      every `kubectl` call. Not covered by `.gitignore` (`git check-ignore` returns 1).
      `git rm -r --cached .kube` and add `/.kube/` to `.gitignore`. **verified again**
- [x] `docs/superpowers/poc/wssnap/` — a standalone POC crate (Cargo.toml + 706-line `main.rs`
      + 86-line `suite.sh`) superseded by `crates/workspaces/src/engine/`. The two prose
      mentions elsewhere name the *VM*, not this directory.
- [x] `docs/superpowers/audit-2026-08-25-raw.md` — 510 lines, self-described "verbatim and
      unedited" inputs to `audit-2026-08-25.md`, itself superseded by `audits/2026-08-29/`.
- [x] `tests/throughput.rs` — 218 lines, three `#[ignore]`d benchmarks with **zero**
      assertions (`grep -c assert` = 0); nothing in `.github/` or `deploy/` invokes them. The
      numbers live in `docs/perf-bench-2026-08-24.md`. **verified again**
- [x] ~~`.cargo/audit.toml`~~ — **REVERTED, the finding was wrong and it broke CI.** The two
      files are read by two DIFFERENT tools: `.cargo/audit.toml` by `cargo audit` (the
      `rustsec/audit-check` action), `deny.toml` by `cargo-deny`. Both run in `image.yml`'s test
      job, so RUSTSEC-2023-0071 has to be ignored in both. Deleting one turned the test job red
      on every commit after it, which also blocks the image job (`needs: [build, test]`). The
      file is restored with a comment saying so.
- [x] `web/apps/web/README.md` — 36 lines of `create-next-app` boilerplate about Geist and
      Vercel deploys, describing files that no longer exist.
- [x] `web/apps/web/public/{next,vercel,file,globe,window}.svg` — create-next-app leftovers,
      zero references in `src`.
- [x] `web/apps/web/public/brand/*.svg` (6 files) — `logo.tsx` inlines its own SVG; no
      `/brand/` path appears in any `.tsx`/`.ts`/`.css`.
- [x] `deploy/RECOVERY.md:359-386` — the "Migrating from the named leader (one-time)" section.
      The StatefulSet it rolls off no longer exists in any manifest and the migration ran on
      2026-08-29.

## Wave 2 — Dead code with zero callers

Each verified by grep across the whole repo including `tests/` and `bins/`.

- [x] **The draining protocol, ~90 lines across five files.** `OwnershipStore::draining()` has
      zero readers — only `set_draining` writes. It fed the name-based leader's `least_loaded`,
      which was deleted with leader election. Cut `DRAIN_PREFIX`, `set_draining`, `draining()`,
      `App::announce_draining`, the `own_draining` handler and its route, the `| "draining"`
      arm, and both `main.rs` call sites.
      [`crates/storage/src/ownership/mod.rs:91,415-458`, `crates/app/src/lib.rs:631-644,653`,
      `bins/server/src/router/route.rs:119-135`, `router/mod.rs:8,62`, `main.rs:99-104,152-157`]
      **verified again**
- [x] ~~`migrate_ws_to_vol`~~ — **KEPT. My "verified again" note was wrong**: the grep behind it
      ended in `head -3`, which hid the real caller at `bins/agent/src/main.rs:26`. The original
      audit was right — it runs on every agent boot. It stays until an operator confirms no node
      has a `{pool}/ws` directory left; that is a cluster check, so it moves to the decision list.
- [x] `pulls::{set_state, finish_merge, clear_merge}` — 45 lines, zero callers anywhere; merge
      outcomes land through the peer-only outcome route. [`crates/pulls/src/pulls/jobs.rs:172-217`]
      **verified again**
- [x] `pulls::claim_merge` — 39 lines. Production claims by number only
      (`claim_merge_number`); the scanner's only callers are `tests/pulls.rs:673,693`.
      **verified again**
- [x] `Directory::all_repos` + the `repos: Collection<Repo>` field, its `db.collection("repos")`
      init and the `Repo` struct — 37 lines, zero call sites; only its own doc comments mention
      the backfill tool it claims to serve. [`crates/pulls/src/directory/mod.rs`] **verified again**
      DONE for `all_repos`; the collection, field and struct STAY — `delete_team` counts them.
- [x] `PushOut`'s `sha`/`raw`/`compressed`/`layers`/`elapsed` fields and `PullOut` entirely, 25
      lines — production reads only `.layer`. [`crates/workspaces/src/engine/ops.rs:85-100`]
- [x] `StoreErr::CasFailed` and `::Conflict` — unconstructible (no `if_match_etag` survives in
      `cosmos.rs`; both `create_*` paths handle 409 before `map_err`), plus `query_items`, a
      generic helper with one caller and a fixed `"SELECT * FROM c"`. 14 lines.
- [ ] `Engine::push` and `Engine::clone_local` — one-line wrappers over `push_env`/
      `clone_local_ids` with zero production callers; tests call the id-taking twins instead.
- [x] `blob::get_bytes` (5 test-only call sites → move into the test file) and `pull_raw`
      (its own doc says "nothing in production calls it"; make `pull_core` `pub(crate)`).
      `get_bytes` done; `pull_raw` STAYS — its caller is an integration test, which `pub(crate)`
      cannot reach. `PullOut` is gone, so it now returns `()`.
- [x] `config::object_store()` — zero callers; everything uses `object_store_views()` or
      `ownership.object_store()`. [`crates/storage/src/config.rs:108-111`]
- [x] `kube_test::conflict()` — zero callers; the conflict-adopt tests build the 409 inline.
- [x] `WsState::Cloning` / `EnvState::Cloning` — never constructed (`crd::Phase` has no
      `Cloning`); also removes the dead `state === "cloning"` branches in four web components.
- [x] `crd::FIELD_MANAGER` (no reader — everything uses `AGENT_FIELD_MANAGER` or a literal),
      `binding::WAIT` (zero callers; both reconcilers use `TICK`), `Config::hostname` (set from
      `HOSTNAME`, never read), `Workspace.live_state` (hardcoded `Value::Null` by its only
      production constructor), the duplicate `nix_claim_name`. **`FIELD_MANAGER` verified again**
      `FIELD_MANAGER`, `WAIT` and `Config::hostname` done. `live_state` STAYS — `engine_ops.rs`
      has three tests on it. `nix_claim_name` STAYS — its twin is `nix_pv_name`; naming a PVC
      after a PV is worse than the duplicate string.
- [x] `orSignIn` (web) — exported with six lines of comment, zero call sites; all six users
      call `listOrSignIn`. Plus the never-imported `SelectScrollUp/DownButton` re-exports.
- [x] Four doc comments left on the wrong item by earlier refactors, and one orphaned
      `GET /v1/repos?owner=X` doc stranded above `feed_get`.
      [`bins/agent/src/controller.rs:215,677,913`, `lib.rs:558`, `crates/api/src/feed.rs:3-5`]

## Wave 3 — Unused dependencies (15)

- [x] `rustls` in `bins/{server,api,worker}` — none of the three names it;
      `install_crypto_provider` lives in `crates/storage/src/config.rs`. Keep the workspace pin.
- [x] Declared-but-never-referenced member deps: `serde_json` (crates/app), `futures`
      (crates/git), `serde` (crates/gitbase), `reqwest` + `chrono` (crates/pulls),
      `kube-runtime` + `reqwest` (bins/agent), `axum` (bins/api), `tracing-subscriber`
      (bins/gateway), `chrono` (bins/server), `redis` (bins/worker).
      NOTE: `kube-runtime` in bins/agent is a FALSE POSITIVE — it is used (feature-gated APIs:
      `reflector::store_shared`, `Writer::subscribe`, `StreamBackoff::reflect_shared`,
      `Controller::for_shared_stream`/`reconcile_on`/`watches_shared_stream` in
      `bins/agent/src/controller.rs`), removing it broke the build with 7 errors — kept.
      `reqwest` in bins/agent was correctly unused and removed.
- [x] `base64` in `bins/agent` is test-only → `[dev-dependencies]`.
- [x] `AWS_PROFILE=do` in `.env.example` — no Rust code reads it and `object_store` 0.14.1
      contains no `AWS_PROFILE` string.

Verify each with a build, not just a grep: a dep can be reached through a macro.

## Wave 4 — Mechanical dedup (same behaviour, fewer lines)

- [x] The four NetworkPolicy builders, **−100 lines** — 186 lines of nested `Some(vec![…])`
      typed structs for four *static* specs. Keep `policy()`, feed it
      `serde_json::from_value(json!({…}))`; only `ns`/`owner` are interpolated.
      [`crates/workspaces/src/k8s.rs:1159-1345`]
- [x] `claim_workspace` / `claim_environment` are the same 42-line function twice — identical
      attempt loop, `decide`/`bound_elsewhere`/`replace_status`/`ensure_binding`/409 arms; only
      the `Api<K>`, `Phase` and log field differ. One generic `claim<K>`. **−40**
- [ ] Three hand-written status comparators (`status_eq` plus the inline field-by-field guards
      in `write_ws_status`/`write_env_status`) all do what `settled_status_eq` already does
      generically. **−35**
      SKIPPED: widening `shape()` to the whole status changes which writes are suppressed.
      `settle`'s per-kind builders are PARTIAL (no `observedGeneration`, no `podRef`), so a
      full-status compare never matches an object that has them — a permanently-failed object
      would then write status on every reconcile, which is exactly the hot loop the comment on
      `settled_status_eq` exists to prevent.
- [x] Three copies of the `read_dir({pool}/vol)` + `is_dir` + `lineage(id)` walk; `cleanup_local`
      scans the pool twice for two projections of one map. Call `read_lineages` once. **−30**
- [x] Five hand-inlined copies of "status + `read_bounded` or log-and-502" → `relay(r).await`,
      which is exactly that function. **−25** [images.rs ×2, repos.rs, browse.rs, signatures.rs]
      THREE done (images.rs ×2, repos.rs). The `browse.rs` and `signatures.rs` copies are NOT
      relays — both read the body to USE it (a cache write, a JSON parse) and build their own
      response afterwards; `relay` consumes the response and returns one. Left alone.
- [x] `Upstream::delete_volume` / `delete_snapshot` — the same 20-line reqwest DELETE twice →
      `delete_ok(as_owner, path)`, mirroring the existing `get_json`. **−25**
- [x] Web: three near-identical `error.tsx` (100 lines of identical eyebrow/title/digest/retry
      markup) → one `ErrorPage({title, body, className})` + three ~8-line exports. **−100**
- [x] Web: the dead `--sidebar-*` and `--chart-1..5` tokens (13 aliases + 39 definitions, zero
      utilities use them) and the unused `@theme` layout tokens. Keep `max-w-auth`.
- [x] Web: `FastRefresh` is `AutoRefresh` with a different default and no visibilitychange
      listener — delete it; its three call sites become `<AutoRefresh intervalMs={2_000} />`.
- [x] Tests: the duplicate `pack_of` helper (move `pack_cap.rs`'s copy into `tests/common`),
      the 7× three-line store preamble in `auth.rs`'s unit tests, `env_obj` re-spelling
      `new_env`'s JSON, the poll-and-promote blocks in `tests/ownership.rs`, `ctx_with_registry`
      and `home_volume()` (one caller each), and the `2xx || 404` assertion that cannot fail.
      **−280 total**
- [ ] Smaller shrinks, ~90 lines: `compress_to_file`'s duplicated drain loop, `stop_workspace`'s
      three identical wait/fail statuses, `ensure_profile`'s four-line failure epilogue ×4, the
      `LabelSelector` literal ×3, `ws_volume`/`env_volume`, `feed_get`'s hand-built peer headers.
      (`feed_get` done — it goes through `forward::to_owner` + `text_bounded` now.)
      All but `feed_get` done (the `LabelSelector` literals went with the NetworkPolicy JSON).

## Wave 5 — Platform and stdlib replacements

- [ ] `RealNix::run` hand-rolls a 200 ms `try_wait` poll loop plus two `std::thread` pipe-drain
      threads. `tokio::process::Command` + `kill_on_drop(true)` +
      `timeout(t, child.wait_with_output())` does all three. Keep `process_group(0)` and the
      `libc::kill(-pid)` grandchild reap. **−35** [`bins/agent/src/nix.rs:106-163`]
      SKIPPED: `Nix::build`/`ping` are SYNC trait methods and `ctx.nix.ping()` is called straight
      from the async reconcile, where `block_on` panics. Needs the whole trait to go async, which
      is not pure motion.
- [ ] The `HostKeys` trait + `SshKeygen` shellout + `FakeHostKeys` + `Ctx::host_keys` field and
      constructor parameter — a single-implementation trait that exists only because the impl
      shells out. `ssh-key` is already in the lock via `russh`:
      `PrivateKey::random(&mut OsRng, Algorithm::Ed25519)` makes it a pure function and the
      trait disappears. **−50**
      SKIPPED: `bins/agent` depends on neither `russh` nor `ssh-key` (only `crates/{api,git}` and
      `bins/server` do), so this needs a NEW dependency entry on this crate.
- [x] Two manual fill-to-`CHUNK` loops → `r.by_ref().take(CHUNK).read_to_end(&mut buf)?`; the
      second one deletes outright, since `WriteMultipart::new_with_chunk_size` already
      accumulates short writes. **−18**
- [ ] Hand-rolled `hex()` (a `write!` per byte) → `hex::encode`; the crate is already resolved
      in `Cargo.lock`. **−12** [`crates/core/src/err.rs:40-49`]
      SKIPPED: `hex` is a transitive resolution only — there is no `[workspace.dependencies]`
      pin for it, so this is a new external dep, not a swap.
- [x] `kube_test::not_found` hand-writes the `Status` JSON literal →
      `Status::failure("not found", "NotFound").with_code(404)`, the constructor already used
      at `api.rs:2003`. **−14**
- [x] Web: `pull-commits.tsx` and `commit-meta.ts` each build their own `Intl.DateTimeFormat`
      per commit; export `lib/time.ts`'s pinned `ABSOLUTE` and reuse it, so the list and the
      detail page cannot disagree.
- [x] Five env knobs read in code but set in no manifest, test or script → `const`:
      `KLOUDLITE_BLOB_GRACE_SECS`, `_SLATEDB_BLOCK_CACHE_MB`, `_META_CACHE_MB`,
      `_FLUSH_INTERVAL_MS`, `_MAX_CONCURRENT_RECEIVE`, `_MERGE_CACHE_BYTES`.
- [x] Deduplicate `scheme`/`user_names`/`GIT_PLACEHOLDER`, which exist in both
      `crates/storage/src/auth.rs` and `crates/core/src/httpx.rs`. The doc's "keep storage
      axum-free" justification is void — storage already depends on core. **−20**
- [x] `forward::read_bounded` is a pure delegate whose only effect is `Vec<u8>` → `Bytes`, and
      every caller wants the `Vec` anyway. `require_jwt_secret` has one non-test caller four
      lines below it. `with_directory` is a builder for one field with one call site that the
      doc says never changes after startup. **−19**
      `read_bounded` done. `require_jwt_secret_from_env` STAYS — it has TWO callers, one per
      binary (`bins/server/src/main.rs:55`, `bins/api/src/main.rs:108`), which is exactly the
      duplication its doc says it exists to prevent. `with_directory` STAYS — folding it into
      `App::new` rewrites five call sites, three of them in `tests/` and
      `crates/workspaces/tests/`, i.e. outside this pass.
- [x] Rename `crates/storage/src/cache/disk.rs` → `streams.rs`. Its own module doc says
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
      `members` list verbatim; it can go only if the root `kloudlite-tests` package and its
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

---

## Postscript — a retracted audit, and what came of it

After the waves landed, the workspaces/agent auditor sent a retraction claiming it had
fabricated its findings, citing three refactors (`claim<K>`, `Upstream::delete_ok`,
`volume_ref`) that "already existed". **The retraction was wrong.** `delete_ok` and
`volume_ref` were created by cleanup commit `c1c0a4d` hours earlier, and `claim_workspace`
went from 47 lines to 15 in `3e0f1e7` — the auditor re-read the tree *after* its own proposals
had been implemented and mistook them for pre-existing code. Its original findings stand; each
was independently re-verified by the implementer before execution, which is why three of them
(the status comparators, `RealNix::run`, `HostKeys`) were skipped with concrete reasons rather
than forced through.

Two genuinely new items came out of that pass:

- [x] `WSSNAP_SQUASH_MB` / `WSSNAP_CHAIN_MAX` — env reads nothing has ever set, in `deploy/`,
      tests or scripts → plain values on the struct. Done.
- [ ] `unescape_mount` + its test (~25 lines, `engine/pool.rs:189-225`) — **kept deliberately.**
      It octal-unescapes `/proc/self/mounts`, where the kernel writes a space as `\040`. The
      audit is right that `WS_POOL` is set once in the DaemonSet and never contains a space, but
      this is parsing a system file at a trust boundary: without it a pool path with a space
      mis-parses silently instead of failing. 25 lines is a cheap price for that.

`bins/agent/src/controller.rs` and `crates/workspaces/src/api.rs` (~1,400 lines) are still
un-audited — the retracting agent's one true admission. Worth a pass of their own.

---

## Second pass — nothing skipped

On the instruction "I don't want you to skip anything", every item left on the decision list and
every earlier skip was re-opened. Cluster facts were checked rather than assumed: no node has a
`{pool}/ws` directory (only `recv`, `stage`, `vol`); no `Deployment` exists in any `ws-*`/`wt-*`/
`env-*` namespace; no Workspace or Environment carries a legacy `spec.nodeName`/`spec.volumeRef`;
no workspace sets `spec.restore`.

Done in this pass:

- [x] `RealNix::run` → `tokio::process` + `kill_on_drop` + `timeout`. The `Nix` trait is now
      async. The old comment rejecting `wait_with_output` applied to **std's** version, which
      needs the pipes untaken; tokio's polls both pipes and the exit together, so the drain
      threads and the 200 ms poll went. The 1 MiB-stderr test still passes.
- [x] `HostKeys` trait + `SshKeygen` shellout → `ssh-key` (exact lock version pinned; no new
      package). Trait, fake and `Ctx::host_keys` gone; `sshkeys::generate()` is a pure function.
- [x] `generate_ed25519`'s `ssh-keygen` shellout → `russh`. The comment defending it (a second
      `rand_core`) was **void**: `rand_core` 0.6.4, 0.9.5 and 0.10.1 are all already in the graph.
      Output verified byte-compatible with `ssh-keygen -y -f` and `-l -f`. `tempfile` dropped.
- [x] `migrate_ws_to_vol`, the Deployment→StatefulSet migration block, `hex::encode`,
      `Engine::push`/`clone_local`, `pull_raw`, cross-region layer stores (with all four doc
      references rewritten), `with_directory` → an `App::new` parameter.
- [x] Web: the radius scale (41 dead `rounded-*` stripped; `rounded-full`, `rounded-[2px]` and
      `rounded-[inherit]` kept — they are not governed by `--radius`), the CI and Issues
      placeholder routes, `NotYet`, the `/actions` redirect, and the landing page's now-untrue
      "CI Triggers" card.
- [x] `WSSNAP_SQUASH_MB` / `WSSNAP_CHAIN_MAX` → plain values.

Investigated and deliberately NOT done, with the reason:

- **The root package and `default-members`.** The duplicate 15-entry list can only go if the root
  `kloudlite-tests` package moves into its own member directory. That means relocating 24 Rust
  test files while `tests/registry_e2e.sh` and `tests/ws_e2e.sh` must stay where CLAUDE.md
  documents them — splitting the suite across two directories, and re-introducing the footgun the
  `default-members` comment exists to prevent (bare `cargo build` at the root, which is the deploy
  image's build command, would build only the test host). 16 duplicated lines is not worth that.
- **`unescape_mount`** — kept. It octal-unescapes `/proc/self/mounts`, where the kernel writes a
  space as `\040`. Parsing a system file is a trust boundary; without it a pool path containing a
  space mis-parses silently.
- **`require_jwt_secret`** — two callers, not one. Collapsing it duplicates the env reads it
  exists to prevent.
- **`forward::read_or_502`** — written, measured at **+6 lines net**, reverted.
- **The three status comparators** — they compare genuinely different field sets (Volume:
  `subvolumePresent`/`lineageTip`/…; Workspace: `podRef`/`nodeName`/…; Environment: `nodeName`/
  `serviceStatus`/…), so they are not one function. What *was* duplicated — the
  `if prev == next` / `Api::all` / `to_value` / `patch_status` shell, three copies — is now one
  generic `write_status`. `settled_status_eq`'s `shape()` was NOT widened: that would break
  write-suppression and spin a permanently-failed object into a status write every reconcile.
- **`PushOut::{sha, layers, squash_triggered}`, `Workspace.live_state`, `nix_claim_name`, the
  `repos` collection, `kube-runtime`** — all reported as dead, all verified to have real callers
  or assertions. Kept.

One assertion was deleted, deliberately and reported: `keys_generate_in_the_cache_dir_and_nowhere_else`
asserted that key generation *fails* when `KLOUDLITE_CACHE_DIR` points at an unwritable directory —
a property that existed only because `ssh-keygen -f` needed a scratch file. With no filesystem in
the path it cannot hold. The format half of the test survives as `keys_are_openssh_format`.

Still open from the `controller.rs`/`api.rs` audit (`.superpowers/cleanup-controller-api-audit.md`,
−149 lines): the legacy `spec.nodeName`/`volumeRef` pointers (precondition now verified clear),
`VolumeStatus.lineage_tip`, `VolumeStatus.progress`, `WorkspaceSpec.restore`, `install_user_key`,
the two `reconcile_*` delegating wrappers, and the drain-poll duplication. `WorkspaceSpec.resources`
is on that list too but should NOT be cut: both live workspaces persist it, and its doc calls it
"the M session slot from the capacity model" — it is the designed sizing knob, not dead config.
