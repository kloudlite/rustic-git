# Review Fixes — Effort/Impact Ordering

Companion to `2026-08-23-review-fixes-index.md`. Same 104 tasks, reordered by payoff. Work top to bottom;
each wave is independently shippable. Task ids are `<plan>/<n>`: reg, http, git, web, ops.

Effort: S ≤ 30 min, M ≤ 2 h, L half-day+. Impact: what it unlocks, not just severity.

## Wave 0 — Developer-time multipliers (do first; every later wave gets cheaper)

| Task | Effort | Why first |
|---|---|---|
| ops/19 Skewable clock + `Pool::await_retires` in `tests/routing.rs` | M | Routing suite 58 s → seconds; removes the flakiest test in the repo. Every Rust wave below runs it repeatedly. |
| ops/2b Clear the 9 `clippy --lib` warnings | S | Prerequisite for a CI gate; done once, stops the "is this warning mine?" question forever. |
| ops/3 CI gate: `cargo test` + `clippy --lib -D warnings` + `cargo audit` | S | Every wave after this is verified on push, not by hand. |
| ops/11 Root `.dockerignore` | S | Each image build stops shipping `target/` + `node_modules` to the daemon. |
| web/1 (test-runner half) `bun test` scaffold | S | First web test harness; passkey fix rides on it. |
| ops/16, ops/17, ops/13 empty test file, e2e trap, gitignore anchors | S | Three 1-line cleanups; zero risk. |

## Wave 1 — Critical + cheap high-impact bugs

| Task | Effort | Impact |
|---|---|---|
| reg/1 Upload session = staging object; worker never opens a DB | M | **Critical.** Stops the worker fencing serving nodes. Also deletes the session-row leak and three phantom-image sites for free. |
| web/1 (fix half) Passkey assertion split | S | **Critical.** Passkey login works at all. |
| http/1 SSH fingerprint case + signature tests | S | SSH-signed commits verify; first-ever `verify_signature` tests. |
| http/2 Peer-only routes refuse session tokens | S | Closes JWT self-renewal and passkey-counter tampering. |
| git/5 Only a fence is reported as a fence | S | Stops needless re-route/evict on every clean close. |
| git/3 Pack cap on every transport | S | Disk-fill DoS over SSH closed. |
| web/6 Drop "Rebase and merge" | S | Users stop getting a silent fast-forward. |
| web/2 Unknown fence lang → text | S | README with ```console no longer 500s the repo page. |
| http/7 Poisoned auth cache → `into_inner` | S | One panic no longer takes auth down for the pod's lifetime. |

## Wave 2 — High-impact, medium effort (correctness of core paths)

| Task | Effort | Impact |
|---|---|---|
| git/1 Incremental fetch sends only additions | M | Fetch bandwidth O(delta) instead of O(repo). Biggest user-visible perf win in the repo. |
| git/2 Filtered pack honours its filter | S | `--filter=blob:none` actually partial. Same code as git/1. |
| reg/3 Manifest PUT verifies blobs; drop copy-to-self | M | No more 201-then-broken-pull; closes the mount race on real S3. |
| git/4 Evicted-during-open DB is closed | M | Closes a writer-epoch leak that can surface as a fence later. |
| web/3 Shell owner from URL; ⌘K real repos | M | Team pages navigable at all. |
| web/4 Team pages work for every team | S | Pairs with web/3. |
| http/4 One fence-retry helper (HTTP/SSH/proxy) | M | SSH stops wedging after a stray fence. |
| http/3 `.git` pull reads don't conjure ghost DBs | S | Unrouted DB creation closed. |
| reg/6 Dead lane kills worker; Redis-down test | S | Worker failures visible; load-bearing fallback finally tested. |
| reg/4 Refuse non-JSON manifests; sha512 probe | S | One garbage PUT can no longer disable GC for an owner. |
| git/8 GPG payload from raw bytes | M | Valid signatures stop reading `Invalid`. |
| ops/1 + ops/2 Non-root image + securityContext | M | Blast-radius of any RCE shrinks from root to uid 1001. |

## Wave 3 — Medium impact, small effort (batch these in one sitting each)

**Rust, one commit each, all S:**
http/5 require `REPLICAS` · http/8 cap description · http/9 `read_bounded` everywhere · http/10 fixed 500 messages · http/13 conditional `claim_username` · http/14 admin visibility via routed endpoint · http/17 validate admin owner · http/18 `expect` on client build · reg/7 manifest DELETE guard · reg/8 GC put_in_place + skip idle sweep · reg/9 reconcile all owners · reg/10 basic-auth username · reg/11 referrers artifactType · git/15 index parse fallback · git/16 log survives bad commit · git/17 `write_err` cap · git/18 ref-name component · git/19 trailing-`*` only · git/13 pipelined cache get

**Web, all S:**
web/10 404 not 500 · web/11 `pathHref` · web/8 file-tree anchors · web/14 button-in-link · web/17 locale/tooltip/provider · web/12 login form · web/9 error + loading pages

**Ops, all S:**
ops/4 ingress `/v2` + ssl-redirect · ops/5 ClusterIP instead of LB http · ops/6 PDBs · ops/7 JWT secret required · ops/9 worker/api probes · ops/10 web health route · ops/8 pin web.yml actions · ops/12 pin digests · ops/14 + ops/15 docs

## Wave 4 — Medium effort, runtime-stall and memory (do when load matters)

| Task | Effort |
|---|---|
| reg/5 Stream blob bodies via multipart | L — the only L. Removes the 10 GiB buffer; needs the new `tests/registry_limits.rs`. |
| git/7 Stream pack download to disk | S |
| git/9 + git/10 + http/11 odb work off the runtime thread | M combined |
| git/11 Walk repo once per fetch | S |
| git/12 Diff size from header | S |
| git/6 Prune stale local packs (with 1 h mtime guard) | M |
| http/12 Concurrent tag reads | S |
| http/16 Negative credential cache | S |
| http/15 `typ: "session"` (invalidates live sessions once — ship with a note) | S |
| web/7 Browse at a commit | M |
| web/13 Destructive actions report errors | M |
| web/2 (lazy grammars + size cap half) | S |
| git/20 Signature time vs key validity | S |

## Wave 5 — Redundancy / dead code (low risk, do opportunistically or in one sweep)

reg/2 + git/21 `hex` helper · http/19 `basic_token`/`unauthorized` · http/20 verify JWT once · http/21 move test helper · http/22 drop `split('?')` · http/23 `leader() -> &str` · http/24 ponytail ceiling · http/6 mutex across await in tests · git/14, git/22, git/23 stale docs + `peel_wants` · git/24 release profile · reg/12, reg/13 pins + comments · web/5 delete mock Compare/Issues · web/15 `guardImage` · web/16 cookie config · web/18 `useCopy` · web/19 one theme control · web/20 delete dev bypass · web/21 radius overrides · web/22 shadcn devDep · ops/18 compat-matrix env · http/25 revoked-credential test

## Suggested first session

ops/19 → ops/2b → ops/3 → reg/1 → web/1 → http/1 → http/2 → git/5 → git/3. That is one day, leaves CI gating every later change, both Criticals closed, and the test suite fast enough to iterate on.
