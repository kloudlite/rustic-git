# Mark (api) + GC (controller) — implementation report

Base 0ce5c8dc. Commits: 9f8000e7 (mark + switch + policy + api RBAC), c5301f6b (controller GC + RBAC),
5c3b7822 (probe, catalogue, slo.md, fixture), 1363a4fd (CLAUDE.md).

## 1. Mark (api) — 9f8000e7
- `crates/workspaces/src/api/membership.rs`: deletes nothing. Stamp writes `removed-at` + `delete-after`
  (removed-at + 7 d) under SSA manager `kloudlite-membership`. `Verdict::Delete` renamed `Due`: past grace or
  delete-now, it writes the full mark only on objects still lacking an api-written `delete-after` (never moves an
  existing one). Clear nulls all three. Removed `cleanup`, `Target`, `CLEANUP_DELETES_PER_BEAT`, the re-judge,
  the budget/409 handling and the attach-policy drop (moved to GC). Keep-biased judging unchanged.
- `removals.rs`: delete-now writes `{removed-at, delete-now: "true", delete-after: now}`; the second
  `reconcile_pair` after the mark is gone. `deletes_enabled` = GET `ClusterSettings/default` merged over
  `AgentSettings::from_env()` (unreadable answers false; response text only).
- `deploy/k3s/agent-admission.yaml`: `kloudlite-removal-stamps-are-the-apis` fences `delete-after` too.
- Tests: stamp carries delete-after = removed-at + grace; due pair marked on bench, 2 workspaces, space (not the
  personal ws) with zero DELETEs; already-marked due pair writes nothing; re-add clears delete-after; delete-now
  writes delete-after ≈ now and no DELETE; `crd_yaml::the_removal_stamp_policy_fences_every_mark`.

## 3. RBAC
- api: `benches` loses `delete` (nothing else in the api deletes a Bench). `workspaces` and `spaceenvironments`
  KEEP `delete`: `delete_ws` (`/v1/workspaces/{id}`), `me.rs` clear choice, `environments.rs` delete_env sweep.
- controller: new rule `delete` on benches/workspaces/spaceenvironments (get/list/watch already there); header
  table updated; networkpolicies `delete` (already granted) now also covers the GC's env-side `attach-{ws}` drop.

## 4. Switch
- `memberRemovalDeletes` removed from central (`CentralSettings`, stored doc, snapshot, META, apply/merge).
  `StoredCentralSettings` has no `deny_unknown_fields`, so a stored document still carrying the key parses.
  The api user-role `refresh_central_beat` spawn and `ApiState.central` existed only for this and are removed.
- Added to `ClusterSettingsSpec` (Mark::Live), `AgentSettings` (env `WS_MEMBER_REMOVAL_DELETES`, default false),
  schema env/default tables, `merge_cluster_spec`, round-trip tests; `deploy/k3s/crds.yaml` regenerated.

## 2. GC — c5301f6b
- `bins/controller/src/gc.rs`, wired into `lib.rs` `select!`; `space::may_write` made `pub(crate)`.
- Every 60 s, only while `ctx.leading()`; lists Bench/Workspace/SpaceEnvironment (any error → `gc.listing.failed`,
  whole pass skipped). Due = api-written (`managedFields` manager) `delete-after` parsed ≤ now, not already
  terminating; a Bench also needs `kloudlite.io/bench-folder` (else `gc.bench_waits_finalizer`). Switch off →
  `gc.would_delete` (kind/name/owner/team/due). On → fresh-lease `may_write` before each delete, uid+rv
  preconditions, 404/409 → `gc.skipped`, ok → `gc.deleted` and drop an attached workspace's env-side policy.
- Tests: due+system-marked deleted with preconditions (not-due, no finalizer, unmarked, foreign-marked kept);
  never touches snapshots/volumes/environments; switch off deletes nothing; listing error skips pass; follower
  deletes nothing; a 409 is skipped and the rest run.

## 5. Probe — 5c3b7822
- `bins/slo/src/stages/monthly/removed.rs`: no up-front central read, no skip. After delete-now:
  `deletes_enabled` true → pair gone within `GC_BOUND` 180 s, snapshot + volume kept, audit rows
  (`judged`, `delete_now`; `member.removed.cleanup` audit row no longer exists — controller has no audit store),
  re-add finds no bench; false → Bench and team Workspace exist with a due api-written `delete-after`, audited.
- SLI wording updated identically in `catalogue.rs`, `deploy/slo.md`, web fixture row.

## 6. Docs — 1363a4fd
CLAUDE.md removal paragraph rewritten (api marks, controller GC, regional switch, why).

## Gates
`cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test` core, workspaces, controller-bin,
api-bin, slo-bin all pass; `cargo test -p kloudlite-tests --no-run` builds; web lint, typecheck, test --force pass.

## Deviations / concerns
- Commits carry no Co-Authored-By trailers: the repo commit-msg hook and the owner's global rule refuse them.
- No `member.removed.cleanup` audit row any more (controller holds no object store); logs `gc.deleted` instead.
- 180 s probe bound includes the agent's bench-folder finalizer and the workspace finalizer's sync-point
  cleanup, not only two GC ticks — may be tight on a slow node; unverified on fleet.
- GC trusts the mark without re-judging the directory: a re-add after the mark is due is undone only if the
  keys beat (≤300 s) clears it before the next GC tick (≤60 s). Per the approved design.
- Roll order: apply the updated admission policy before the controller has `delete`; any object carrying
  `removed-at` without `delete-after` gets marked by the beat once due. Stored central `memberRemovalDeletes`
  is now ignored; turn it on per region in `ClusterSettings`.
- Web fixture comment (superadmin.ts:82) still names `member_removal_deletes`; comment only, left.

## Fix round: immediate clear on re-join — 593bd655
- `crates/api/src/teams/pause.rs`: the on_member_state call moved into `reconcile_member` (same 20 s
  `RECONCILE_WAIT`); pause/unpause use it unchanged.
- `crates/api/src/teams.rs::accept_invite`: on `Joined`, awaits `reconcile_member` before answering, so
  `membership::reconcile_pair` clears removed-at/delete-now/delete-after before the 200 (not the 300 s beat).
- Invite accept is the ONLY path in crates/api that makes someone a member (no direct admin-add route;
  set_role needs an existing member).
- Test: `accepting_an_invite_reconciles_the_pair_once` (hook called exactly once with (handle, team)).
- Fixture comment superadmin.ts:82 now names the regional `memberRemovalDeletes`.
- NOT done: superadmin `Directory::grant_access` (access Request approve, `crates/workspaces/src/api/admin.rs`)
  runs in the ADMIN process, which reaches the cluster as `kloudlite-admin`; the stamp admission policy
  admits only `kloudlite-api` and that SA has no Bench patch, so an immediate reconcile there would be refused.
  That path still clears on the keys beat (≤300 s) and can race the GC for a pair already due. Fix options
  (owner call): admit kloudlite-admin in the policy + RBAC, or have the admin approve call the user-role api.
- Gates: clippy --workspace --all-targets clean; kloudlite-api, workspaces, api-bin tests pass;
  kloudlite-tests --no-run builds; web lint/typecheck/test pass.
