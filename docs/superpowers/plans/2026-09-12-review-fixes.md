# Review fixes implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan batch by batch. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every finding of the 2026-09-12 whole-tree review (241 findings, 41 high) in an order that removes the security and data-loss items first and ships each tier once per phase.

**Architecture:** Fixes are grouped into batches by crate so one batch is one commit, one ship, one roll of the tiers it touches, and one direct verification on the fleet. Security and data-loss findings go first regardless of size; performance and redundancy follow. Probe-only batches roll only the probe image. Every batch adds the test that would have caught the finding.

**Tech Stack:** Rust workspace (edit in the dev pod at `/work/src`), `deploy/dev/ship.sh` gate, `deploy/pin.sh` + `deploy/roll.sh` + k3s apply, probe ids in `crates/workspaces/src/slo/catalogue.rs` mirrored by `deploy/slo.md` and the console fixture.

**Review:** the findings are numbered as on the review page (rustic-git Review 2026-09-12, three waves). Finding numbers below are those numbers.

## Global constraints

- Every edit lands in the dev pod (`/work/src`) through `kubectl exec -i … bash -s`; the laptop only pulls. Never `cargo` on the Mac.
- Never edit in the pod while `ship` is building. One batch = one commit = one ship.
- Never a plain `cargo update`; only `cargo update -p <crate>`.
- Commit subjects: imperative, sentence case, no tool attribution.
- Push `origin master` and `platform master` only.
- A batch is done when its change is verified on the fleet on the carrying build: a probe id passed, or a direct `/v1`/`kubectl exec` check, never "the cron will tell us".
- A finding that changes a probe assertion also updates `deploy/slo.md` and `web/apps/web/src/lib/fixtures/superadmin.ts` (the ship gate holds them equal).
- A finding marked *design* below is closed by a comment stating the decision, not by a code change, unless the owner says otherwise.

## Order of phases

| Phase | Theme | Tiers rolled | Batches |
|---|---|---|---|
| 0 | Security and data loss | api, server, agent, k3s RBAC, web | 0.1–0.7 |
| 1 | Tool server (`crates/ide`) | workspace image | 1.1 |
| 2 | Probe correctness and drills | probe image | 2.1–2.3 |
| 3 | Agent correctness and performance | agent | 3.1–3.3 |
| 4 | Workspaces API correctness and performance | api, admin | 4.1–4.2 |
| 5 | Server, storage, registry, pulls | server, worker | 5.1–5.3 |
| 6 | Deploy and operability | manifests only | 6.1–6.2 |
| 7 | Web | web | 7.1 |
| 8 | Tests | none | 8.1 |
| 9 | Redundancy sweep | all, one ship | 9.1 |

Phase 0 ships as soon as each batch is green; later phases may be reordered by the owner. Each batch below: findings, files, the change, the test, the fleet check.

---

## Phase 0 — security and data loss

### Batch 0.1 — quota accounting for teams (#1, #2, #12, #117)

**Files:** `crates/workspaces/src/api/workspaces/mod.rs`, `api/push.rs`, `api/clone_restore.rs`, `quota.rs`, `api/admin/owners.rs`, `crates/workspaces/tests/api_quota.rs`

- [ ] Decide the owner stamp: team objects get `spec.owner = team` (environments already do). `guard_alloc`'s `owner_of` and the object's `owner` field become one value in create, clone, restore, push.
- [ ] `quota::usage` keeps summing `spec.owner`; nothing else changes there.
- [ ] `owners.rs::fold_usage` calls the same per-item rules as `quota::usage` (skip `spec.system`, per-service resources) — extract `quota::cost_of_env` and use it in both.
- [ ] Tests: a team create raises the team's usage and refuses at the team's limit; a person's create does not count against the team; two creates with a changed listing between them recompute (pins "never cached", #117).
- [ ] Fleet check: create a team workspace via `/v1` with a minted token, `GET /v1/quota?owner=<team>` shows the count; hourly `team.*` ids pass.

### Batch 0.2 — identity and keys (#103, #104, #105, #121, gpg expiry-0, #4 peer identity case, #5 approve limiter)

**Files:** `crates/api/src/credentials.rs`, `teams.rs`, `gpg.rs`, `lib.rs`, `crates/pulls/src/directory/credentials.rs`

- [ ] `revoke` takes the kind from the row's id prefix (`sign:` → `SigningKey`); `DELETE /v1/keys/{id}` revokes both kinds; test: register a signing key, delete it, commits no longer verify.
- [ ] Sign-in: `is_superadmin` error → log `directory.read.failed` and answer 502; test with a failing directory double.
- [ ] `signer_by_any`: match full fingerprints for attribution; key ids only narrow; more than one row → `None` with a warn. Test: two keys sharing a key id.
- [ ] `effective_expiry`: exclude `CertRevocation` from the uid iterator; `verified_emails` drops revoked uids; `KeyExpirationTime(0)` means no expiry. Tests from fixtures in `gpg.rs` tests.
- [ ] `peer_only` lowercases the asserted identity like `mint` does.
- [ ] `per_ip` limiter on `/v1/cli/approve` and `/v1/cli/code/{code}`.
- [ ] Fleet check: hourly `commit.verify`, `id.*`, `key.*` ids pass; a signing-key delete over `/v1/keys` answers 204.

### Batch 0.3 — git tree and fetch bounds (#2, #3, #120, #122, walk memo)

**Files:** `crates/gitbase/src/objects.rs`, `crates/git/src/browse.rs`, `crates/git/src/protocol/upload/{mod,walk}.rs`

- [ ] `is_dotgit_variant`: git's full HFS-ignorable table; trim trailing `.`/space before the `.`/`..` comparison; reject any component containing `:`. Table-driven test with the git test vectors (`.g\u{00ad}it`, `".. "`, `.git::$DATA`, `GIT~1`).
- [ ] `changed_files`: explicit stack with a depth cap of 4096 and an entry cap passed from `diff_trees_inner`; error past either.
- [ ] Upload: budget the unknown-want closure and the shallow walk with the existing `WALKED` counter; interrupt checked in the shallow loop; memoize commit time in `deepen-since`. Test: a bogus want against a fixture repo answers an error under the budget.
- [ ] Fleet check: fast `git.*` and `ssh.*` ids pass; `deploy/dev/run-job.sh fast`.

### Batch 0.4 — agent data safety (#4, #5, #36, #124, #125, #30)

**Files:** `bins/agent/src/peer/pull.rs`, `peer/sweeps.rs`, `controller/volume.rs`, `controller/environment/intercept.rs`

- [ ] `pull_one`: compute `after - before` on success; refuse and delete unless exactly `[name]`.
- [ ] `delete_subvolume` returns the status; a failed cleanup is logged and the name is excluded from `have`.
- [ ] `sweep_orphan_snapshots` deletes with `Preconditions { uid }`.
- [ ] `attach_volume` empty arm: JSON-patch `test` that the key is absent, then `add`; 422/409 → not attached, retry.
- [ ] Intercept release: propagate the Workspace GET error.
- [ ] Periodic collector uses `beat.all_parents` with `names_volume`.
- [ ] Tests in `bins/agent/tests/reconcile/`: a receive that produces a stranger name is refused; a failed cleanup keeps the name out of the replica status; two attaches to one detached volume keep both entries.
- [ ] Fleet check: hourly `ws.replicated`, `vol.*`, `env.intercept.*` ids pass.

### Batch 0.5 — server routes and worker paths (#106, #127, #128, #24, #27, #21)

**Files:** `bins/worker/src/main.rs`, `bins/server/src/browse_api/{admin,merge,pulls}.rs`, `crates/pulls/src/merge_worker.rs`

- [ ] `handle_event`: `parse_repo_path(&e.repo)` before `sync_branches`; drop the event with a warn otherwise.
- [ ] `api_delete`: `Err` from `repo_exists` → 503.
- [ ] `api_merge`, `api_patch`, `api_protect`: `open_ro` gate (or a doc paragraph stating peer-only is the design — owner decides; default: gate).
- [ ] Worker client build error is a boot failure.
- [ ] `api_pull_check` takes `as_owner`.
- [ ] Networked push stderr scrubbed of `http.extraHeader` and the secret before it becomes an outcome.
- [ ] Fleet check: fast `pr.*`, `git.*` ids; a merge through the web still lands.

### Batch 0.6 — RBAC and edge (#6, #7, #63, #109, #14, #108 partial)

**Files:** `deploy/k3s/slo-rbac.yaml`, `agent-rbac.yaml`, `agent-admission.yaml`, `deploy/clickstack/clickstack-values.yaml`

- [ ] Remove the probe's `serviceaccounts: impersonate` grant; `sec.agent.spec` asserts the admission refusal with a `SelfSubjectAccessReview` or a purpose-made SA (probe change ships in 2.1; the RBAC change waits for it).
- [ ] Add the probe SA to the admission policy `matchConditions` so its `workspaces: patch` is fenced; add the missing rows to the role's header table.
- [ ] `endpoints delete` row added to the agent role table and to the admission DELETE fence.
- [ ] Delete the `kloudlite-slo-drill` Role and binding (drill is a skip).
- [ ] HyperDX ingress: `limit-rps` pair plus `whitelist-source-range` from `ADMIN_CIDR`.
- [ ] Apply on k3s and AKS; fleet check: fast run `yielded`/passes with zero `rollout.check.failed`; HyperDX still reachable from the operator CIDR.

### Batch 0.7 — web security (#74, #75, #76)

**Files:** `web/apps/web/src/lib/docs.ts`, `app/api/repos/route.ts`, `auth.ts`, `welcome/actions.ts`

- [ ] Docs slug: reject anything outside `/^[a-z0-9/-]*$/i` and assert the resolved path stays under root.
- [ ] `/api/repos`: `cache-control: private, no-store`.
- [ ] Session `update` trigger: accept `apiToken` only when the patch carries a server-only marker minted in `welcome/actions.ts`; typed `SessionPatch` on both sides (#78).
- [ ] `bun test` and typecheck in the pod; fleet check: sign-in and docs pages load; `web.*` fast ids pass.

---

## Phase 1 — tool server (`crates/ide`)

### Batch 1.1 (#8, #9, #10, #43–#51, #93, LOW ide items)

**Files:** `crates/ide/src/{paths,procs,graft}.rs`, `tools/{mod,exec,files,patch,watch,graft}.rs`, `fs/{mod,git}.rs`, `server.rs`

- [ ] `job()` reads both streams into `Ring` (bounded), never `read_to_end`.
- [ ] `/fs/stat` sniffs 8 KiB via `File::open` + `read` inside the existing `spawn_blocking`.
- [ ] `grep` skips entries over `MAX_BYTES`.
- [ ] `confine` returns the resolved path; callers open it.
- [ ] argv arrays: a non-string entry is `Invalid`; one helper shared by `exec` and `watch`.
- [ ] `numstat` takes the computed `Vec<Change>`; one status walk per `/fs/changes`.
- [ ] `Registry` builds a name → set index once.
- [ ] `patch` restores written files on a later failure, as `edit` does.
- [ ] Temp file name carries pid + counter.
- [ ] `process_write`: stdin behind `tokio::sync::Mutex<Option<ChildStdin>>`.
- [ ] `Graft::call` removes its pending entry on timeout; watcher calls `is_ignored_dir`; `which` checks the executable bit; `rand_id` and the dead `opt_u64` line deleted; schema table checked once against the child's `tools/list` with a warn on mismatch.
- [ ] Explicit `DefaultBodyLimit::max(MAX_BYTES + 1 MiB)` on `/tools/*` and `/fs/*` (closes the 2 MiB gap the open question found).
- [ ] Tests for each in the crate; fleet check over the tunnel with the existing `verify.py` script plus `exec {"cmd":"yes"}` bounded, and hourly `ide.*` ids.

---

## Phase 2 — probe

### Batch 2.1 — drills leave no state and no cross-suite damage (#107, #130, #131, #38, #133 partial, drain/gateway items)

**Files:** `bins/slo/src/drill.rs`, `stages/mod.rs`, `stages/weekly.rs`, `weekly_gaps/{drain,gateway,registry}.rs`, `monthly/nodes.rs`

- [ ] Taint value and NetworkPolicy names carry `run-{id}`; `sweep_nodes`/`undo_drills` sweep only this run's or entries older than `STALE_SECS`.
- [ ] Decommission and cordon marks go on the node as a `kloudlite.io/slo-drill=run-{id}` label; sweep by label, not the emptyDir file.
- [ ] `ws.cross.node`: stop-poll inside the undo body; step cap sized to poll + body + slack.
- [ ] `stale()` requires the name's suite to equal `c.suite`.
- [ ] `gw.caps` uses `step_cap` and stops the workspace in a compensation; the 5 MiB blob delete runs in `undoing`.
- [ ] `reg.moved.image` uses `roll.zero.errors`' roll instead of deleting every srv pod; taint patch becomes a JSON-patch of the one taint.
- [ ] Tests: `stale` refuses a sibling suite; sweep ignores a taint owned by another run.
- [ ] Fleet check: `run-job.sh fast` during a hand-started weekly step leaves its taint; weekly run passes.

### Batch 2.2 — assertions and skips (#110, #111, #112, #113, #134, #39, #42, B1/B6/B7/B9 of wave one, #6 gaps2, drain #17, commit.verify)

**Files:** `bins/slo/src/{step,report,suite,ctx}.rs`, `stages/{workspace,environment,lifecycle,edge,admin,weekly,experience_gaps2,experience_teams/repos}.rs`, `catalogue.rs`, `deploy/slo.md`, fixture

- [ ] Roll guard: at most one downgrade window per run (a roll that was already in flight at boot yields; a roll that starts mid-run skips the rest of that stage only); the failure detail is kept behind the reason; `rollout_in_flight` cached 10 s.
- [ ] `StepReport` gets a `reason` enum; `report.rs` matches it, not a substring.
- [ ] Kube-less halves (`sts_ready`, `named_by_a_replica`) skip with a reason; `git.push.large` without a host key skips; `history.series.allowlist` on 503 skips.
- [ ] ProxyCommand id validated `^[a-z0-9-]+$`; `path_of` redacts `/v1/invites/` and `/v1/cli/` segments; `call` never quotes bodies of credential-minting URLs.
- [ ] `env.exec` detail names stdout; `edge.origin` accepts only app statuses; `edge.dns` per-host timeout = ceiling / hosts, host named; `builder.hidden` gains a positive control; `commit.verify` requires the oid.
- [ ] `srv.drain.handover`: require `draining` or a refused connection; `git.limits` renamed `reg.limits` in the catalogue.
- [ ] Ceilings: fail a step past its catalogue target while keeping the ceiling as the timeout (B6); stage budget guard at 700 s (B5).
- [ ] Fleet check: `run-job.sh fast` and `hourly` pass; console fixture rows match `slo.md` (ship gate).

### Batch 2.3 — probe credentials, RBAC use and performance (#40, #41, #114, #115 n/a, P1–P8, V4–V6, #132, weekly sleeps, listings)

**Files:** `bins/slo/src/{ctx,crane}.rs`, `stages/mod.rs`, `stages/lifecycle.rs`, `weekly_gaps/{agent,registry}.rs`, `monthly/{nodes,outages}.rs`, `deploy/k3s/slo-rbac.yaml`

- [ ] Admin token minted lazily at the first admin call; routine reads use a non-superadmin admin token.
- [ ] crane login through a written `config.json`; CLI token files 0600 and removed in `undoing`.
- [ ] Registry upload `Location` must share the registry's origin.
- [ ] `poll_json`/`wait_for`: 250 ms for 5 s then 1 s; `volume_gone` 2 s; whole-volume listing reused across the three `wt.delete` checks; `ws.replicated` and snapshot reads field-selected; `sweep_requests` field-selected; the three two-minute sleeps become `settle()`.
- [ ] Redis outage drill selects one srv pod; monthly `idle_node` lists pool nodes first.
- [ ] `edge.rs` dial via `TcpStream::connect`, no bash.
- [ ] RBAC: after 2.1 and 2.2 land, drop `nodes: patch` to a label-fenced grant and the direct StatefulSet patch / pod delete grants (drills go through `/admin/workloads/.../roll`).
- [ ] Fleet check: fast/hourly/weekly runs pass; probe pod count of API calls per run drops (count `slo.http.*` lines).

---

## Phase 3 — agent

### Batch 3.1 — correctness (#28, #33, #35, #37, stop.rs #9, mounts #10, intercept port #11, wishes index #12)

- [ ] `btrfs receive` child bounded by `send_timeout`; janitor `btrfs subvolume delete` under `timeout -s KILL`; peer send handler's Volume GET bounded and "unknown quota" distinguished from zero; `REPAIR` mutex skips within a short window after an attempt.
- [ ] `stop_environment` runs teardown and `replicated_condition` before stamping `observed_generation` on a repeated stop.
- [ ] `mounts.rs` validates every mount before dedup; `intercept_plan` validates `PortMap` (1..=65535, port present) and settles the wish `Off` with a reason; `wishes.get` instead of index.
- [ ] Tests in `bins/agent/tests/reconcile/` for each.
- [ ] Fleet check: hourly `env.*`, `ws.stop.p95`, `ws.start.p95` pass; no `reconcile.slow` from these steps.

### Batch 3.2 — performance (#29, #31, #32, #34, #126, intercept GETs, StatefulSet GETs, claim listings, quota per pass)

- [ ] `my_node` from a reflector store; sync beat lists Snapshots once; `parents_on_volume` from stores; sweeps gate the re-read GET on stale worktrees; blocking fs work moved into the existing `spawn_blocking` batches (pull, keys, worktree, profile).
- [ ] Environment: one StatefulSet list per pass; intercept reads Workspace and Pod from stores; `drain_services` probes twice then requeues; attach-policy prune moves to the sweep beat; fabric phase runs only on generation change; one node/pod snapshot shared by `nodes_with_room` and `roomiest`.
- [ ] Fleet check: k3s API server request rate from the agent (`kube-apiserver` metrics in ClickStack) drops; hourly passes.

### Batch 3.3 — redundancy (#90, #91, comment duplicates)

- [ ] Delete the three `#[allow(unused_imports)]` re-export blocks; `#[cfg(test)]` the test seams; `.parent().unwrap()` → `ok_or_else`; drop the duplicated comment blocks in `environment/run.rs`.

---

## Phase 4 — workspaces API

### Batch 4.1 — correctness (#11, #12 done in 0.1, #59, #57, #58, #62, #14 my_ws, #21 volumes owner rule)

- [ ] Settings: `roll_readers` error logged and carried into the audit result.
- [ ] `my_ws` superadmin arm goes through `may_act_on` (logs `superadmin.acting`).
- [ ] Drop `max_per_owner`, `home_cache_gb`, `stop_flush_timeout_secs` (CRD field, range, schema, `over!`, console column) or wire readers — owner decides; default: drop.
- [ ] Schema env column labelled "this process's env"; cluster list returns partial rows with a per-row error instead of 500.
- [ ] `volumes.rs` `?owner=` path expressed through `caller_owners`.
- [ ] Fleet check: admin console pages render; `req.*`, `settings.*` ids pass.

### Batch 4.2 — performance (#52–#56, #60, #61, #115, #9 serial awaits, #19 packages client, #20 history expect)

- [ ] `is_team` batched over the owner set; cluster detail reads five kinds from `fleet::all`; one Region fetch per route; name → node map; `try_join!` in the quota gate and inside `usage`; `spawn_blocking` around the btrfs and flock calls in `engine/ops.rs`; `.to_str().unwrap()` → `EngErr`; CLI-token revocation cache 30 s; fleet reads return `Arc<K>`; one shared package-index client; `History::new` returns `Option`; `authorized_keys` projection one `$in` query.
- [ ] Fleet check: `/admin/owners` and `/admin/clusters/{region}` latency in `http.slow` logs drops; `audit.row`/`tel.pod.coverage` fast ids pass.

---

## Phase 5 — server, storage, registry, pulls

### Batch 5.1 — correctness (#15, #16, #80, #81, #129, is_public marker #11, receive error #15, pack cleanup log #16, remove_protection #6, refs unwrap #18, index decode #7)

- [ ] `bound_dead_peer` on accepted peer streams; `keyed_lock` and the pool/cache mutexes use one `lock_or_recover`; `stats_of` errors skip not zero; refuse boot without `KLOUDLITE_JWT_SECRET` when not solo; merge lane keeps pacing on the empty path; `is_public` error skips the marker write; receive propagates a body read error; failed pack cleanup logged with paths; `remove_protection` validates the pattern; `head_target` no `unwrap`; index decode stops at the first `description`.
- [ ] Fleet check: fast `git.*`, `reg.*`, `pr.*` pass; `deploy/dev/run-job.sh fast`.

### Batch 5.2 — bounds and paging (#18, #19, #20, #82, image write body limits #8, images O(n²) #6, volumes paging #7, api_files cap #12, api_refs cap #15, index list bound #8, #9, catalog/referrers `n`, deepen-not exact #2, ownership #3/#4/#5/#12)

- [ ] `_catalog` windows before `stats_of`; referrers honour `n` with `Link`; `check_repo` bounds the scan; sha512 probe only when such a manifest exists; `DefaultBodyLimit` on the image write routes; `images` slices markers; `volumes`/`volumehistory` paginate; `api_files` sizes opt-in; `api_refs` clamped; `index::list` capped and paged with start-after; `deepen-not` resolves under standard prefixes with one ref listing.
- [ ] Ownership map: `WRITE_BOUND < LEASE_TTL`; prune pass bounded as a whole; epoch re-checked after a bounded write; `now_ms` below a compiled-in floor refuses to grant; `all()` scans without the role lock held.
- [ ] Tests for each cap; fleet check: registry catalog with `n=2` costs two stats (log count); routing tests.

### Batch 5.3 — directory and worker (#14 mongo timeouts, #10 compound index, #9 unbounded listings, #13 regex migration gate, #13 last-superadmin CAS, #14 audit-before-grant, #25 const rename, #26 kl-connect PATH check, #83–#89)

- [ ] Mongo client: connect, server-selection and per-query `max_time`; `{owner, kind, createdAt}` index; listings limited and cursored; migration behind the sentinel; `remove_superadmin` conditional delete; audit row written before the grant; `KLOUDLITE_MERGE_CACHE_BYTES` read from env or renamed; `on_path("kl-connect")`; target percent-encoded; ws id DNS-label check in the gateway; `NamedTempFile` and `--` in `kl`; heartbeat via `tokio::fs`; `$HOSTNAME` instead of forking; gateway frame copy via `Bytes`.
- [ ] Fleet check: sign-in and CLI login flows; hourly `id.*` pass.

---

## Phase 6 — deploy and operability (#64–#73, #96–#98, dev script items)

- [ ] Agent DaemonSet: mount the pool device, not `/dev`; readiness probe; explicit `updateStrategy`; comment on `WS_RUNTIME_CLASS` corrected.
- [ ] Gateway: readiness probe; memory limit above request.
- [ ] Worker PDB or the explicit trade-off comment; otel collectors get the four-line `securityContext`; `/var/log/pods` mount dropped.
- [ ] Workspace image base pinned by digest; comment on the missing `USER`.
- [ ] Secrets as projected files for `kloudlite-jwt`/`kloudlite-peer` (code reads a path with env fallback) — its own batch, rolls every tier.
- [ ] OTel stack: one ConfigMap for the three differing values, or a test diffing the two files.
- [ ] Shared-password sign-in keys deleted once the OAuth provider is live (owner decision).
- [ ] Cloudflare Full (strict) for registry and HyperDX hosts with `ssl-redirect: "true"` in the same change (owner decision; verify with `dig` first).
- [ ] `api-rbac` `pods: list` narrowed via an agent-side occupancy check.
- [ ] `deploy/dev/slo.sh` deleted or documented; `default-members` dropped from `Cargo.toml`; `RUNBOOK.local.md` moved.
- [ ] Verify: roll, `deploy/dev/run-job.sh fast` and `hourly`.

## Phase 7 — web (#13, #77–#79, #99–#102, quota-bar, markdown stacks)

- [ ] `cache()` on `page`, `nav`, `searchIndex`; drop the duplicate existence check; `app/docs/error.tsx`.
- [ ] Install command from `centralSettings().cloneHost`; `.catch` on the two floating promises; `Promise.all` on the registries page; palette keeps `repos` keyed by owner.
- [ ] Metrics gate requires a private source or token; docs renderer escapes `href`/`title`.
- [ ] Delete `QuotaBar`; README through the `marked` pipeline, drop `react-markdown`/`remark-gfm`.
- [ ] `bun run typecheck`, `bun test`, `bun run build` in the pod; bundle size recorded (closes the last uncovered item).

## Phase 8 — tests (#116, #118, #119, silent skips, weak assertions)

- [ ] Replace the eight wall-clock sleeps with condition polls (`wait_idle` shape); serialize or inject the `KLOUDLITE_MAX_*` env writes; `crates/workspaces/tests/common/mod.rs` with `token`, `jwt`, `admin_token`, `get_json`, `delete`.
- [ ] `a_plain_stopped_workspace_still_starts` asserts the patch body; `an_annotation_keyed_digest…` reads the blob rows; `drop_snapshot…no_op` asserts the other subvolumes remain; the 59 `skipping:` returns become `#[ignore]` with a reason where the prerequisite is environmental, so a green run means what it says.
- [ ] Ship gate must still pass at 1899+ tests.

## Phase 9 — redundancy sweep (every remaining LOW: probe helper copies, `ceil_div`, `epoch`, `azure_rule`, `hourly_in_flight`, `compat-matrix.sh`, `merge::perform` inline, `imagedelete` purge helper, seed-key echo, `DEFAULT_BRANCH` duplicate, `volumes.rs` frozen surface note)

- [ ] One commit per crate group; the ship gate is the test.

---

## Self-review against the review page

- Every HIGH (#1–#14, #103–#119, #120–#134) is named in a Phase 0–2 batch. `imagedelete` on read error stays a design note (rejected item).
- Every MEDIUM is in Phases 1–7 by area; every LOW in Phase 9 or beside its area's batch.
- Owner decisions flagged: signing-key revoke shape (0.2), `open_ro` on merge/patch/protect (0.5), dropping the three dead settings knobs (4.1), shared-password keys and Cloudflare SSL mode (6), secrets as files (6).
- Open question carried: Next.js `..` normalization — the containment check in 0.7 closes it regardless.
