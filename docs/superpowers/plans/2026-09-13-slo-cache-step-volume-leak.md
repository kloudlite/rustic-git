# SLO probe: `ws.cache.travels` leaks a detached volume every hour

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax. Edit on the laptop, build/test/ship from the dev pod (`/work/src`); nothing is fixed until it passed on the fleet on the carrying build.

**Goal:** `vol.history` (hourly, `deploy/slo.md:174`) passes again and stays passing: the hourly tenant `slo-hourly` stops accumulating one detached volume and snapshot per run from `ws.cache.travels` (`deploy/slo.md:151`), and the 19 already leaked are removed.

**Root cause (diagnosis: scratchpad `slo-failures.md`, cause A, confirmed against the code):**
- `cache_in_tree` (`bins/slo/src/stages/experience_ws.rs:314-345`) creates `run-{id}-cache`, then `push_then_restore` (`:410-423`) pushes it with body `{}` (`:412`), a non-transient snapshot. It then deletes both workspaces (`:330`, `:333`). Only the restored workspace is registered, in `extra_workspaces` (`:419`). The volume, named by the source workspace id, is never registered in `extra_volumes`.
- Once its parents are gone the volume is detached and kept alive by the push. `/v1/volumes` then reports `display_name` = the volume id (`crates/workspaces/src/api/volumes.rs:310`, `unwrap_or_else(|| name.clone())`). Teardown's prefix sweep matches volumes on `display_name` (`bins/slo/src/stages/mod.rs`, the `KINDS` volume entry), and boot's `stale` sweep (`mod.rs:878-886`) needs a `run-` name, so neither can ever see it.
- `drop_extra_volumes` (`mod.rs:519-526`) would have deleted it: it runs after the sweep in `teardown` (`mod.rs:356`). `vol.history`'s own `prepare` registers its volume for exactly this reason (`bins/slo/src/stages/experience_env.rs:302-304`, test assert `:551`).

**Architecture:** reuse the existing seam. Register the volume in `c.state.extra_volumes` the moment it exists, so teardown takes it whatever the step did. Add one keep-biased backstop that lets the prefix and stale sweeps recognise a detached probe volume: the probe names its pushes with the run prefix, because the snapshot message is the only caller-chosen string that survives a detach (`volumes.rs:556` returns `message` in `/history`).

## Decisions

1. **Register, don't delete in the step.** `DELETE /v1/volumes/{name}` 409s while any parent exists (`volumes.rs:352-354`), and the restored workspace's finalizer is still running right after `drop_ws`. An in-step delete would therefore mostly fail. Teardown's `drop_extra_volumes` runs after the sweep, which is the ordering that works. The step still calls a best-effort delete after both `drop_ws` calls, on success and on failure. Its failure is only logged, and teardown is the guarantee.
2. **Register before the push, right after `create` answers.** A push that lands and then times out in `poll_json` still holds a snapshot. That is the same "recorded BEFORE the wait" rule `env_restore` follows (`experience_env.rs` restore step).
3. **Backstop keys on the push message, never on age or owner alone.** A detached volume is swept only if every one of the following holds:
   - `/v1/volumes` says `deleted: true`;
   - its `/history` read succeeded and is non-empty;
   - every row's `message` satisfies the sweep's predicate: `starts_with(prefix)` at teardown, `stale(...)` at boot.

   Any read error, an empty history, or a single non-matching message keeps the volume. `vol.history`'s pushes ("one"/"two") never match, and they are registered anyway.
4. **The 19 existing leaks carry no message** (pushed with `{}`), so the backstop cannot see them. They go through a one-time, owner-approved runbook (Task 3), not through code that widens the sweep to message-less volumes.

## Global Constraints

- Keep-biased: nothing is deleted unless this run registered it or its every snapshot names a `run-` prefix of this suite. No sweep on "detached and old".
- No product change. `/v1` behaves as documented (detach keeps the volume, quota counts it).
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test -p kloudlite-slo` in the pod before pushing.
- Independent of the `bench-*` branches. Branch from `master`, not from `bench-platform`.
- Commit subjects are imperative sentence case with no attribution to Claude.

---

## Task 1: Register the cache step's volume and name its push

**Files:** `bins/slo/src/stages/experience_ws.rs`

- [ ] In `cache_in_tree`, immediately after `let src = create(...)?` (`:322`), add `c.state.extra_volumes.push(src.clone());` with a WHY comment: the volume outlives both workspaces once pushed, and is named by id, so no prefix sweep sees it.
- [ ] Give `push_then_restore` a `message: &str` parameter and push `json!({ "message": message })` instead of `json!({})` (`:412`). `cache_in_tree` passes `&name` (`run-{id}-cache`).
- [ ] After `drop_ws(c, &restored)` (`:333`), and on the early-return paths at `:326` and after `out?` fails, make one best-effort `DELETE /v1/volumes/{src}` through the existing logging pattern of `drop_ws`. No `?`: a 409 while the finalizer runs is expected, and teardown retries it.
- [ ] **Tests** (same file, `mod tests`, `testkit::ctx_against`, kube set to the dummy client as at `:903`):
  - `cache_travels_registers_its_volume_before_the_push`: POST `/v1/workspaces` answers `{"id":"ws-1"}`, the exec route is not needed because `push` 500s, so the step fails. Assert `c.state.extra_volumes.contains("ws-1")` and exactly one failed `ws.cache.travels` row.

    The exec happens over kube, so if the dummy client makes `ws_exec` fail before the push, assert registration anyway: it happens before the exec. That is the point of Decision 2.
  - `cache_travels_pushes_under_the_run_prefix`: capture the push body in the fake router. Assert `message` starts with `c.prefix()`. Skip this test if it needs a real exec; fall back to a unit test on `push_then_restore` directly against a router with push/history/restore/get routes.
- [ ] Commit: `Register the cache step's volume for teardown and name its push`

## Task 2: Sweep detached volumes whose every push names a run

**Files:** `bins/slo/src/stages/mod.rs`

- [ ] Add `async fn sweep_detached_volumes<M: Fn(&str) -> bool>(c, owner, jwt, matches) -> usize`:
  - GET `/v1/volumes` and keep rows with `deleted == true`.
  - For each row, GET `/v1/volumes/{name}/history`, then `DELETE /v1/volumes/{name}` via `del` (`mod.rs:686`) only if the rows are non-empty and every `message` satisfies `matches`.
  - Every list or read error: log `slo.teardown.failed` and `continue`.
- [ ] Call it from `sweep` (`mod.rs:579`) after the `KINDS` loop, so both `teardown` (prefix, `:354`) and `boot` (`stale`, `:340`) get it for both tenants.
- [ ] **Tests** (`mod.rs` `mod tests`, `testkit::ctx_against`):
  - `a_detached_volume_whose_pushes_all_name_this_run_is_swept`: `/v1/volumes` lists `ws-a` deleted, history `[{"message":"run-hourly-1-cache"}]`, and the DELETE route counts calls. Assert one DELETE on `ws-a`.
  - `a_detached_volume_is_kept_on_any_doubt`: four volumes, each asserted to receive zero DELETEs:
    - `ws-b`: one message not matching;
    - `ws-c`: empty message;
    - `ws-d`: history 500;
    - `ws-e`: `deleted: false` with matching messages.
- [ ] Commit: `Sweep detached probe volumes whose every push names the run`

## Task 3 (runbook): remove the 19 leaked volumes — REQUIRES THE OWNER'S GO-AHEAD BEFORE ANY DELETE

Read-only until step 3. Run from the dev pod against the k3s region.

1. **List and prove.** Run one kubectl query and print one row per volume, with these conditions:
   - `kubectl get snapshots.kloudlite.io -o json | jq` selecting `spec.owner=="slo-hourly"`, `spec.transient==false`, grouped by `spec.volume`;
   - joined with `kubectl get volumes.kloudlite.io <v> -o json`, which must show `metadata.ownerReferences` empty or absent;
   - the volume has no Workspace naming it (`kubectl get workspaces -o json | jq` on `spec.volume` / `status`);
   - every snapshot on it has an empty `spec.message`;
   - its creation time is at hh:03–04 UTC, between 2026-09-12 21:00 and the ship.

   Print `volume, snapshot count, snapshot names, creationTimestamp, ownerRefs count, message`.
2. **Cross-check against the api log.** For each volume id, ClickStack `default.otel_logs` must show `POST /v1/workspaces/{id}/push` followed within ~5 s by `DELETE /v1/workspaces/{id}`, inside an hourly run window whose probe row logged `ws.cache.travels`. Save the table plus the log match as the dry-run output and show it to the owner. Expected: exactly 19 rows. Any other count, any ownerReference, any message, any live parent: stop and report.
3. **Delete, one at a time, on approval.**
   - Mint an `slo-hourly` session with the probe's own `kloudlite-jwt` secret, the way `Ctx` mints (`bins/slo/src/ctx.rs`, `jwt`/`admin_jwt` at `:259`; the user token path, not the superadmin one).
   - For each volume: `curl -sS -X DELETE -H "authorization: Bearer $T" $API/v1/volumes/{name} -w '%{http_code}'`.
   - Expect `204`. On any other code (409 "someone else's snapshots", 409 "still has a workspace", 404), stop the loop and report.
   - After each 204, `kubectl get volume {name}` should be NotFound within a few seconds.
4. **After.** `GET /v1/quota` as `slo-hourly` shows `used.snapshots` well under 20.

## Task 4: Ship and verify

- [ ] Workflow (`CLAUDE.md` "Deploying", memory "Edit locally, ship from pod"):
  1. Edit on the laptop and push the `platform` branch.
  2. `deploy/dev/sync.sh` then `deploy/dev/test.sh` in the pod: slo crate tests plus `clippy --all-targets`.
  3. Ship the slo image from the pod (`deploy/dev/ship.sh`).
  4. `deploy/pin.sh <sha>` and `deploy/roll.sh`. Only the `kloudlite-slo` CronJob images change.
  5. Push origin master only after verification below.
- [ ] Run Task 3 (approved) before or right after the roll. `vol.history` cannot pass until the quota has room.
- [ ] Verify directly, not by waiting for the hourly:
  1. Launch one hourly run now: `deploy/dev/run-job.sh` / `kubectl create job --from=cronjob/<hourly>`.
  2. Expect `ws.cache.travels ok` and `vol.history ok` in its `slo.step.done` log rows.
  3. Expect teardown `slo.teardown.deleted kind=volume` naming the cache volume id.
  4. With a minted `slo-hourly` token, `GET /v1/volumes` shows no `deleted: true` row, and `GET /v1/quota` shows `used.snapshots` unchanged from before the run.
  5. Repeat on the next natural hourly. Say "changed, unverified" until both hold.
