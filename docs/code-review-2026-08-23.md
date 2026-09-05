# kloudlite full code review — 2026-08-23

Scope: all of `src/`, `tests/`, `web/apps/web`, `deploy/`, CI, Dockerfiles, docs. Five independent
passes (HTTP/auth/api, registry/GC/worker, git core/storage, web app, ops/CI/tests). Every finding
below was verified by reading the code (and vendored crate source where a library claim mattered).

Baseline: `cargo test` green (6 documented benchmark ignores). `bun run lint` and `tsc` clean.
Clippy: 15 pre-existing `--all-targets` warnings (list at the end).

---

## 0. Top 10 — fix these first

| # | Sev | Where | What |
|---|-----|-------|------|
| 1 | **Critical** | `src/registry/uploads.rs:395` ← `src/bin/worker.rs:283` | `sweep_stale_uploads` opens the image SlateDB from the **worker** to delete the `upload/{uuid}` row. Violates the single-opener invariant → fences the owning node (`detected newer DB client`). Fires on every GC pass for any owner with an upload >24h old. |
| 2 | **Critical** | `web/.../lib/passkey.ts:85-93` | Assertion is `"${email}.${exp}.${mac}"` and verifier `split(".")` requires exactly 3 parts. Every email has a dot → **passkey sign-in can never succeed**. |
| 3 | High | `src/api/credentials.rs:246` + `src/directory.rs:577` | SSH fingerprints stored mixed-case, looked up lowercased → **SSH commit signatures always `unknown_key`**. No test covers `verify_signature`. |
| 4 | High | `src/protocol/upload.rs:812-818` | `ObjectExpansion::TreeContents` + hiding only commits → every incremental `git fetch` re-sends the whole tree snapshot. Bandwidth O(repo), not O(delta). |
| 5 | High | `src/protocol/receive.rs:350` + `src/ssh.rs` | **No pack-size limit over SSH** (HTTP has `max_body`). Authenticated pusher can fill node disk. |
| 6 | High | `src/registry/manifests.rs:49-164` | Manifest PUT never verifies referenced blobs exist (no `MANIFEST_BLOB_UNKNOWN`). Combined with the 1h `blob_grace` sweep, slow pushes of big images lose early layers and still return 201. |
| 7 | High | `src/registry/blobs.rs:79` | `refresh_blob_mtime` = `copy(path, path)`; real S3 rejects same-key copy without metadata directive, error swallowed → the "mount race" is only closed on `mem://`/`file://`. |
| 8 | High | `src/registry/{blobs,uploads}.rs` | Blob bodies are buffered as `Bytes` up to `max_layer` (10 GiB); PATCH re-reads + rewrites the whole staging object. Memory O(layer) per request, OOM with a few concurrent multi-GB pushes. |
| 9 | High | `deploy/kloudlite.yaml` + root `Dockerfile` | Rust workloads run as **root with all caps** (no `securityContext`, no `USER`); web pod already has the hardened pattern. |
| 10 | High | `.github/workflows/image.yml` | **`cargo test`/clippy/fmt never run in CI.** Images ship untested. |

---

## 1. Security

### High
- **Root containers** — `deploy/kloudlite.yaml` (leader, srv, api, worker) and `Dockerfile:28-43`. Copy `kloudlite-web.yaml:105-108` securityContext + add `USER`; chown cache/data dirs.
- **SSH push unbounded** — see Top 10 #5. Wrap input in a counting reader capped at `max_body`.
- **Registry ingress** `deploy/kloudlite.yaml:744-750` routes `/` (whole git surface + `/healthz`) on `cr.khost.dev`, with `ssl-redirect: "false"` on a host *not* behind Cloudflare → plain-HTTP Basic creds accepted. Path `/v2` + ssl-redirect true on this ingress.
- **`kloudlite-lb`** (`:460-476`) exposes HTTP :80 directly, bypassing ingress/TLS. Drop the port or document why.

### Medium
- **Leaked session JWT renews forever** — `src/api/teams.rs:84-118`. `upsert_user` accepts a Bearer and mints a fresh 12h token if `body.email == sub`. Reject Bearer on this route (peer-secret only).
- **"PEER ONLY" passkey routes reachable with any session** — `src/api/passkeys.rs:125-174`. Any user can read another's passkey pubkey/email and set `counter` arbitrarily (breaks victim's next login via clone detection). Add a `peer_only()` caller variant.
- **Self-hosted runner** on `workflow_dispatch` workflow with persistent docker cache and GHCR write token (`image.yml:12`). Acceptable for solo repo; document; prune cache on schedule.
- **Actions pinned by tag not SHA** (`image.yml`, `web.yml`).
- `KLOUDLITE_JWT_SECRET` is `optional: true` in yaml (`:86,303`; web `:113`). Make required.

### Low
- `src/jwt.rs:70-110` — session claims have no `typ`/`aud`; rejection of registry tokens relies on serde requiring `name`. Add `typ: "session"`.
- `src/api/mod.rs:196-211` — failed JWT falls through to `owner_for_token`, uncached miss = S3 GET per bogus token. Negative LRU or rate limit.
- `src/gpg.rs:141-183` — signature creation time not checked against key creation/expiry; `signature_expiration_time` ignored.
- `src/registry/auth.rs:45-49` — Basic username ignored; scheme match is case-sensitive (RFC 7235 says insensitive).
- `web/.../(auth)/actions.ts:11-13` — `provider` from form passed unvalidated to `signIn()` (nil impact today).
- No `cargo audit`/`cargo deny` anywhere; `rsa 0.10.0-rc.18` (via `russh`/`ssh-key` rc) on the SSH auth path.

---

## 2. Bugs (correctness)

### Critical / High
- Worker opens image DB (Top 10 #1). Fix: worker deletes staging object only; owner drops the row lazily when `received()` finds no staging object.
- Passkey split (Top 10 #2). `lastIndexOf` twice or base64url the email.
- SSH fingerprint case (Top 10 #3). Lowercase at registration + backfill.
- Fetch sends full snapshot (Top 10 #4). `TreeAdditionsComparedToAncestor`.
- Manifest PUT skips blob existence (Top 10 #6). `head()` each digest from the same walk `gc::collect` uses; also the natural place for the mtime touch.
- `refresh_blob_mtime` no-op on S3 (Top 10 #7).
- **README fence with unknown language 500s the repo page** — `web/.../repo/code.tsx:34` casts `as BundledLanguage`; shiki throws on `console`, `jsonc`, `mermaid`. Fall back to `"text"`.
- **App shell always shows personal namespace on team pages** — `app-shell.tsx:50-80`. Derive owner from pathname in `place()`.
- **Team org pages 404 for every team** — `(org)/{settings,ci,environments,workspaces}/page.tsx` check `owner !== session.user.owner`. Drop it; let api 404.
- **"Rebase and merge" silently does fast-forward** — `pull-actions.tsx:85` vs `pulls/actions.ts:57`. Remove the option.

### Medium
- `src/http/browse_api/pulls.rs:96,118` — `api_pulls`/`api_pull` pass raw `name` (with `.git`) to `ready()` → creates ghost DB `repo/alice/web.git` under an unrouted key. Use the parsed `Repo`.
- `src/ssh.rs:221` + `src/proxy.rs:297` — no `on_fenced` retry on SSH paths; a stray fence makes SSH fail until an HTTP request evicts the handle.
- `src/pool.rs:257-278` — `evict` during in-flight open leaks an `Arc<Db>` in no map, never closed, holding the writer epoch. Re-check map membership after `get_or_try_init`.
- `src/pool.rs:196-204` — any `close_reason` (incl. `Clean`) reported as fenced → needless re-route/evict. Match `CloseReason::Fenced` only.
- `src/store.rs:226-268` — `open_repo` never removes stale local `.pack/.idx`; after move→repack→move-back, pruned objects stay servable and disk never reclaimed.
- `src/browse.rs:388-391` — GPG payload rebuilt via canonical re-serialisation, not cut from raw bytes → valid commits with unusual encodings read `Invalid`.
- `src/bin/worker.rs:102-107` — "a lane that dies takes the worker with it" is false: handles awaited sequentially; panicked lane silently reduces capacity. `select_all` + exit.
- `src/registry/manifests.rs:99` + `gc.rs:60-65` — non-JSON manifest accepted; then sweep aborts forever for that owner. 400 `MANIFEST_INVALID`.
- `src/registry/uploads.rs:374-401` — sessions opened but never PATCHed leak rows forever.
- `src/registry/uploads.rs:134,221,260`, `manifests.rs:281-293` — PATCH/GET/DELETE upload and DELETE manifest call `image_db` on nonexistent images → phantom image with private marker appears in listing. Guard with `image_exists`.
- `src/registry/gc.rs:151-167` — case (a) uses `index::write` (delete-then-write) racing `set_image_visibility`; use `put_in_place` like case (c).
- `src/directory.rs:379-406` — `claim_username` check-then-reserve race leaks a handle.
- `src/objects.rs:120-128` — two merges with identical staged content race on the same `incoming-{hash}.pack` path.
- Web: `pull-commits.tsx:77` "browse at this commit" → `/tree` 404 and `?ref=<oid>` never resolves; `pull-files.tsx:48` anchors to ids never rendered; `pull-data.ts:19`, `file-view.tsx:45`, `diff.tsx:35` turn 404 into 500; file paths interpolated into hrefs unencoded (7 sites); `login-form.tsx` dead "Continue to org" button and `/reset` 404 link; password step shown even when disabled.
- Web: ⌘K search (`global-search.tsx`) and Issues/Compare tabs (`issues.tsx`, `compare.tsx`) render **hard-coded mock data** linking to a non-existent `kloudlite` repo.

### Low
- `tests/registry_e2e.sh:33,82` — second `trap` replaces first; `$blob` leaks.
- `src/store.rs:284` — unparseable pack index value → size 0 → re-download every open.
- `src/browse.rs:197` — one malformed commit header 500s the whole log page.
- `src/pktline.rs:112` — `write_err` skips the 0xffff length check.
- `src/protocol/receive.rs:185` — `refs/heads` (no component) passes `valid_ref_name`.
- `src/refs.rs:391-405` — `*` allowed anywhere in protection pattern but only trailing `*` matches.
- `web/.../image-list.tsx:67-83` — `<button>` nested inside `<a>`.
- `web/.../commit-meta.ts:23` — server-side `toLocaleDateString(undefined)` uses pod locale (siblings pin `"en"`).

---

## 3. Performance / resource

- Blob uploads buffered in memory up to 10 GiB (Top 10 #8). Stream into `put_multipart`, persist part list in session row.
- `src/store.rs:335` — `fetch_pack_file` reads whole pack into RAM; ×8 via `buffer_unordered`. Pipe stream to file.
- `src/protocol/upload.rs:265,297,342` — `reachable_set` (full-repo walk) computed up to 3× per fetch.
- `src/objects.rs:114-155` (`write_pack_of_objects` from merge), `src/refs.rs:129` (`is_ancestor` 50k walk), `browse_api/merge.rs:116,286` — sync git work on async runtime threads. `spawn_blocking`.
- `src/browse.rs:534-547` — 4 MiB diff cap checked *after* inflating both blobs fully.
- `src/registry/gc.rs:36-73` — every manifest read twice per ~65s per owner, forever, even when idle.
- `src/registry/manifests.rs:83-91` — two extra `head()`s per by-tag push for a never-happens sha512 case.
- `src/http/browse_api/images.rs:101-127` — 4 sequential round trips per tag. `buffer_unordered(8)`.
- `src/cache.rs:131-139` — two Redis RTTs per `get`; pipeline.
- `web/.../lib/highlight.ts` — 40 grammars eagerly loaded on first highlight; no size cap on highlighted blob.
- No root `.dockerignore` → `target/`, `.git`, `web/node_modules` sent to daemon every build.
- No `[profile.release]` (no `lto`, `codegen-units=1`, `strip`).

---

## 4. Redundancy / dead code

**Rust**
- Basic-auth token extraction ×3: `http.rs:660`, `api/browse.rs:110`, `registry/auth.rs:45`; `unauthorized()` ×2 byte-identical.
- Hex encoder ×4: `gpg.rs:76`, `registry/uploads.rs:46`, `registry/store.rs:48,59`.
- `upload.rs:665,710,774` — "peel wants to commits" loop ×3.
- `registry/uploads.rs` — Content-Range parse and "read staging + append" copy-pasted between `patch` and `complete`.
- `registry/store.rs:46-64` — `Digest::of` re-implements `of_algo("sha256")`.
- `http.rs:702-717` inlines `reopen_after_fence` (:758).
- `api/pulls.rs`, `api/signatures.rs` — JWT verified 2–3× per request (`caller` then `settings_caller` then `commit_patch`).
- `api/pulls.rs:39-69,251-291` — `#[cfg(test)] publish_pull_event` and tests that test only the helper.
- `http.rs:334` — `tail.split('?')` on a path that never has a query.
- `lib.rs:214` — `leader()` returns `Result` that can't fail.
- `registry/blobs.rs:29-38` — `max_layer_is_stable` asserts `x == x`.
- `tests/api_routes.rs` — 0-byte file compiled as an empty test binary.
- `tests/compat-matrix.sh` — reads undocumented `tests/tok2`, port 8081; superseded by `http_e2e.rs`?
- `upload.rs:268` — `// ponytail: no ref-in-want, no include-tag` — both implemented.

**Web**
- Seven copy-to-clipboard widgets (`copy-button`, `command-block`, `remote-picker`, `clone-menu`, `image-list`, `new-token-dialog`, `file-actions`), each with an uncleaned `setTimeout`. One `useCopy()`.
- Two theme selectors (`theme-picker.tsx`, `theme-toggle.tsx`).
- `lib/mock.ts` (`ACTIVITY`, `ENVIRONMENTS`, `FEED`, types), all of `lib/mock-repo.ts` — mostly zero importers; the rest feed fake UI.
- `compare/` route duplicates `pulls/new` with a form that has no action.
- `lib/dev-auth.ts` — bypass yields a session with no `username`/`apiToken`; unusable past the landing page.
- Three identical 15-line preambles in `registries/[image]/{page,tags,settings}` — wants a `guardImage` + layout like the repo side.
- `updateProfile`, `updateTeam`, `inviteMember` are no-ops; `team-settings/triggers/environments/workspaces` render mock rows; `<a href="#">Open</a>`.
- `command.tsx:28,57,74` — `rounded-xl!` overrides that only resolve to 0 by accident.
- `shadcn` CLI in runtime `dependencies` (only needed for the CSS import).

---

## 5. Quality / best practice

- **Stale docs/comments** (all verified wrong): README leader derivation & "scaling is replicas alone" (`README:25,49,200`); yaml leader comment blocks (`:29-33`, `:245-250`); `ownership.rs:227-246` "compaction off"; `events.rs:1-6` "scan Mongo"; `cache.rs:326` "claims against Mongo"; `receive.rs:246` "fork network shares pool"; `main.rs:634-666` "no routed endpoint" (one exists: `imagevisibility`); `registry/store.rs:286` `ponytail:` comment placed between doc comment and fn.
- Undocumented env: `KLOUDLITE_LEADER`, `KLOUDLITE_REPLICAS`, `KLOUDLITE_SERVER_PREFIX`, `KLOUDLITE_UPLOAD_GRACE_SECS`, `KLOUDLITE_WORKER_CONCURRENCY`.
- `main.rs:104-107` — `KLOUDLITE_REPLICAS` silently defaults to 1 in fleet mode → leader hands everything to `srv-0`. Require it when `PEER_SVC` set.
- `main.rs:577-583` — `admin add-token/add-key` accept any owner string.
- `auth.rs:42-100` — `Mutex::lock().unwrap()` on auth cache: poisoned lock = panic every request.
- `main.rs:745,775` — clippy `await_holding_lock`: std Mutex guard held across `.await`.
- `api/mod.rs:103` — `reqwest` builder `.unwrap_or_default()` silently drops the timeout.
- `api/repos.rs:308` — description travels in query string with no length cap (2 MiB body → 6 MiB URL → opaque 502).
- `api/forward.rs:92`, `signatures.rs:99`, `feed.rs:22`, `main.rs:631` — unbounded `.text()` where `read_bounded` exists.
- `browse_api/admin.rs:68,154,168,200`, `merge.rs:97` — raw `e.to_string()` in 500 bodies; `merge` `boom` surfaces in UI.
- `registry/referrers.rs:47` — emits `"artifactType": null` (spec: omit when absent).
- Dependency hygiene: 644 crates, 152 duplicated (`rand` 0.8/0.9/0.10, `rsa` ×2, `hashbrown` ×4, `syn` ×2).
- Floating base image tags with no digest (`rust:1-bookworm`, `debian:bookworm-slim`, `node:22`, `oven/bun:1.3` vs CI's 1.3.14).
- Deploy: no PDB, anti-affinity only `preferred`, no `priorityClassName` on leader; worker has no probes and `emptyDir` without `sizeLimit`; api has readiness only; no CPU limits anywhere (undocumented); web probes hit full `/login` render every 5s.
- `.gitignore:9-10` — unanchored `host_key*`/`cache*`.
- `web.yml:23-28` — `actions/cache` keyed on `github.sha` never hits exactly; churns cache.
- Web: no `error.tsx`/`loading.tsx` anywhere → api outage shows Next's raw error page; destructive actions (`removeTag`, `removeSshKey`, `revokeToken`, `removePasskey`, `removeRule`, `close`) swallow errors and have no confirm; filter inputs with no handlers; `TooltipTrigger` on non-focusable `<span>` (a11y); `api-token.ts` re-derives cookie name from `AUTH_URL` instead of Auth.js config; `?from=expired` set in 8 places, read nowhere.

---

## 6. Test coverage gaps

- `verify_signature` (SSH *and* GPG path via API) — zero tests; this is how #3 survived.
- `api/passkeys.rs`, `api/signatures.rs` — not mentioned in `tests/`.
- `api/teams.rs`, `api/credentials.rs` — no negative paths (wrong team, revoked cred).
- `upsert_user` via Bearer (the renewal hole); `.git`-suffix browse reads; SSH after fence.
- Redis-down fallback for the worker lanes — CLAUDE.md calls it load-bearing; only `events::publish` is covered.
- Git `gc.rs`, `refs.rs` — no dedicated test file; `valid_ref_name` untested as a unit.
- Pack with holes rejected; `--filter` clone round-trip; **incremental fetch pack size** (would have caught #4).
- Registry: PATCH without `Content-Range`; `?tag=` on by-digest push; `unindex`; `_catalog` pagination/`n=0`; `tags/list` on missing image; 413 on oversized blob/manifest; anon HEAD on public blob; DELETE blob unauth; sha512 by-digest-then-tag.
- Flaky pattern: 15 wall-clock `sleep`s in `tests/routing.rs` (58s suite), plus `browse_http.rs:193`, `registry_blobs.rs:274,288`. Use the `MockSystemClock` already pulled in by ea4f7c8.
- `tests/registry_store.rs:12` asserts the wrong reason ("unsupported algorithm" for a length failure).
- `web/` — no tests at all.

---

## 7. Missing features (noticed, not requested)

**Git**: default-branch delete protection; per-repo default branch; `push-options` read but unused; partial clone only `blob:none/limit`, `tree:0`; `deepen-not` with nonexistent oid silently accepted; SSH drain on roll; SIGINT handling (only SIGTERM).
**Registry**: HTTP `Range` on blob GET (containerd resume); `?mount=` without `from`; referrers pagination; `ETag`/`If-None-Match` on manifest GET; `MANIFEST_BLOB_UNKNOWN`.
**Auth**: JWT revocation/refresh; token expiry/last-used; rate limiting/lockout on credential lookups; structured logging / request ids.
**Ops**: CI test gate; `cargo audit`; PDB/HPA/topology spread; metrics endpoint; image signing/provenance; GitOps instead of hand-edited tags (the `ponytail:` note predicted the drift that has now happened).
**Web**: public/anonymous repo browsing (api supports it, UI forbids); session-expired message; profile/team editing; issues; image visibility toggle; "Open in workspace"; view repo at a commit; pagination for PRs/feed; comment timestamps (`ApiComment.at` never passed).

---

## 8. Strengths (keep doing this)

- Routing-before-auth invariant held rigorously; `every_browse_route_is_routable` pins router to `BROWSE_TAILS`; `trust_nobody` strips hop headers on the public listener.
- `Digest::parse` really is the only path→key mapping; manifests verbatim; only two blob deleters, and tests pin both.
- Ownership state machine is pure + explicit clock, exhaustively tested incl. real SlateDB WAL/compaction reclamation.
- `Pool`: single-flight open, refcount-as-guard, `evict_if_same`, never self-reopens a fenced handle.
- Push path: connectivity + isolation check, rejected batches delete their packs, ref updates one serialisable txn with protection enforced inside `update_refs` for all three entry points.
- Constant-time secret compare refusing empties; HS256 pinned; `alg: none` tested.
- GPG: self-sig-only trust, revocations, subkey binding + back-sig, each tested.
- Web: api token never reaches the browser; no raw HTML except shiki output; no open redirects; tokens over raw colours everywhere; `ApiResult` union keeps "expired" vs "empty" distinct by design.
- Deploy: image SHAs pinned consistently with git log; all secrets via `secretKeyRef`; web pod fully hardened; NetworkPolicy scopes peer ports.

---

## Appendix — clippy `--all-targets` (15 pre-existing)

4× `result_large_err` (`api/mod.rs:196,225`, `api/credentials.rs:59`, `http/browse_api/mod.rs:90`); 2× `implied_bounds_in_impls` (`objects.rs:232,310`); 2× `items_after_test_module` (`registry/blobs.rs:30`, `http/browse_api/images.rs:236`); 2× `await_holding_lock` (`main.rs:745,775`); `redundant_closure` (`upload.rs:414`); `useless_vec`/`unnecessary_sort_by` (`api/feed.rs`); `unnecessary_get_then_check` (`admin.rs:255`); `manual_contains` (`gpg.rs:468`); `len_zero` (`tests/browse_http.rs:232`); `permissions_set_readonly_false` (`tests/routing.rs:1712`).
