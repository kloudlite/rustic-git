# Task 15 report — SLO `team.member.removed.cleanup` and `team.member.removed.dir_down`

## What changed
- `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`: two rows at the end of the Monthly (drills) stage `13 · Monthly`, feature Teams, `avail(99.9)` (the brief gives no bound).
- `bins/slo/src/stages/monthly/removed.rs` (new), wired as `member_removed` at the end of `monthly::run`:
  - Untimed prep: own team `run-{id}-rmv` (the probe creates it; the second member is invited and accepts), the member's team bench ready, a team workspace ready, one push, snapshot `ready` in `/v1/volumes/{vol}/history`.
  - Removal (untimed): `DELETE /v1/teams/{team}/members/{email}`, poll the Bench until it carries `kloudlite.io/removed-at` (≤ 600 s), then `POST .../members/{owner}/delete-now` `{person, team}` with the admin JWT. If `deletes_enabled == false`, the id is SKIPPED: "memberRemovalDeletes is off on this fleet: delete-now only marked the pair". The setting is not touched.
  - Step (ceiling 720 s): poll up to 600 s (two beats) until all of these hold:
    - The Bench CR is gone.
    - The team Workspace CR is gone.
    - No SpaceEnvironment remains for (owner, team).
    - No transient Snapshot remains with `worktree == ws`.
    - The pushed Snapshot is still present.
    - Its Volume is still present.
  - Then audit rows `member.removed.judged`, `member.removed.delete_now` and `member.removed.cleanup` for target `{team}/{owner}` via `/admin/audit` (same read as `audit.row`). Then re-add the person and check `GET /v1/bench?team=` answers 404.
  - Teardown (best effort): remove the member, delete the workspace, `DELETE /v1/volumes/{vol}/snapshots/{snap}` for the kept snapshot, delete the team, `env_intercept::delete_members_now`. The `run-` prefix sweep takes any leftovers.
- `team.member.removed.dir_down`: SKIPPED always, reason "no directory fault hook in the drills suite: nothing points the api's directory address at a black hole for one beat (filed)". `drill.rs` only has taint/cordon/decommission/netpol hooks, and no drill targets the directory. No injector was built.
- No hourly group assignment: the drills suite (monthly) is not an Indexed Job, so `suite::group_of` does not apply.
- Tests: `monthly_produces_every_id_once` extended with both ids; `removed.rs` has a catalogue/stage check and a no-kubeconfig skip-once test for both ids.

## Deviations
- Bench folder: no probe pod mounts the share root (workspace pods see only `{pool}/homes/{owner}`). The check is that the Bench CR is GONE, which cannot happen while the `kloudlite.io/bench-folder` finalizer is set, so the folder's absence is INFERRED from it.
- No canary write: the folder is not readable from the probe, so a canary would prove nothing.
- The snapshot teardown deletes by the snapshot id the push returned (the server names it) rather than a `run-{id}` name. The volume's `run-` name is what the next run's sweep would match.

## Verification
- `cargo test -p kloudlite-workspaces slo`: 11 passed (catalogue == slo.md included).
- `cargo test -p kloudlite-slo-bin`: 152 passed.
- `cargo clippy -p kloudlite-slo-bin -p kloudlite-workspaces --all-targets -- -D warnings`: clean.
- Not run against the fleet.

## Concerns
- With `memberRemovalDeletes` off on the fleet, cleanup will skip every month until the setting is flipped.
- dir_down needs a directory fault hook (e.g. a netpol denying api→directory egress through `drill::with_netpol`), filed and not built.
- The audit `action` filter is assumed to be an exact match, and `target` is sent URL-encoded (`%2F`).

## Fix round 1

- **Q1+Q2, skip decided first.** Before anything is created, `cleanup` reads `GET /admin/settings/central` with the admin token. If `memberRemovalDeletes` is not stored as `true`, the id skips with "memberRemovalDeletes is off on this fleet: a removal only marks the pair". The stored document leaves an unset field out and the compiled-in default is off, so a missing field counts as off (`deletes_on`, unit-tested). If the read fails, the step is recorded as failed. The `deletes_enabled` check on the delete-now answer stays as a second guard, with the same reason.
- **Teardown.** The workspace and the kept snapshot are deleted as `probe_jwt`, and both deletes run BEFORE the member removal. Nothing is deleted with the removed member's token.
- **Run-prefixed workspace id: not possible.** `create_ws` names the id `rid("ws")` on the server and the create body has no id field. The workspace NAME (`run-{id}-rmv-ws`) and the push message (`run-{id}-rmv`) carry the prefix instead. The push message is what `sweep_detached_volumes` matches on.
- **`sweep_teams`.** Before `drain_team` (which deletes the team's workspaces), it now calls `sweep_detached_volumes(c, &slug, jwt, matches)`, so a crashed run's detached volume is collected by the next run. No ordering unit test: `sweep_teams` is HTTP-only against a live api and has no test seam today, and building a mock api for it is out of scope.
- **Q5.** The re-add check now requires 404 AND a body containing "no bench".
- **Q6.** "(filed)" removed from the dir_down reason.
- **Verification.** `cargo test -p kloudlite-workspaces slo` 11 passed; `cargo test -p kloudlite-slo-bin` 153 passed; clippy with `-D warnings` clean.
- **Concerns.**
  - `sweep_detached_volumes(c, &slug, …)` lists `/v1/volumes?owner={team}`. A team workspace's Volume may be owned by the PERSON rather than the team, and then this finds nothing. The per-owner sweep over the member still catches it, because the history messages carry the prefix.
  - `probe_jwt` deleting another member's team workspace and snapshot assumes the team owner is allowed to. If the api refuses, the next run's sweep collects them.
