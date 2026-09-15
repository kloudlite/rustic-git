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
