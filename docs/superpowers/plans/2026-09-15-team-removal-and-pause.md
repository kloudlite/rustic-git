# Team member removal and pause — implementation plan

Spec: `docs/superpowers/specs/2026-09-14-bench-tool-credential-design.md`, "Membership lifecycle"
and decisions 8–11 (c346e26f). Builds on `2026-09-15-bench-tool-credential.md`: its check 4
`bench_admits_tool` requires `access == Full`, so a `Paused` bench refuses the tool token with no
change there.

Goal: a removed member's bench, bench folder, team Workspaces, space choice and key projections
are deleted by a keep-biased beat after a 7-day grace, with team-owned data (pushed snapshots, repos,
images, environments) untouched; a paused member keeps data, loses access everywhere, and has their
running bench and team workspaces stopped.

Rules for every task:
- TDD: failing test first, red, implement, green.
- `cargo test -p <crate>` for the touched crate, then
  `cargo clippy --workspace --all-targets -- -D warnings`. Web: `cd web && bun run typecheck && bun run test`.
- One commit per task, imperative sentence case subject, no tool attribution.
- Keep-biased everywhere: a directory error, a timeout or `Source::Unavailable` deletes nothing
  and stops nothing.

Rollout (spec "Rollout order" 1, 2, 3, 4, 7): Tasks 1–3 directory (inert until someone pauses).
Tasks 4–9 api, reconcile in dry-run. Task 10 gateway. Task 11 agent. Tasks 12–13 web. Tasks 14–15
SLO. Task 16 turns deletes on after the owner reads a week of `member.removed.judged` rows.

Owner decision (15 Sep): the bench folder is deleted by a mark the cluster controller owns, not by
an agent inferring orphans. Task 11 uses a finalizer on the Bench itself
(`kloudlite.io/bench-folder`, added at Bench create): the Bench stays in `Terminating` after its
`DELETE` until an agent removes `{pool}/homes/.benches/{team}/{owner}` and clears the finalizer.
Homes are region-shared and every agent mounts the export, so more than one agent's controller can
see the same deletionTimestamp; that is fine by construction (see Task 11) rather than by a claim
one agent wins and the rest skip.

---

## Task 1 — directory `Member.state` and `set_member_state`

Files: `crates/pulls/src/directory/mod.rs`, `crates/pulls/src/directory/teams.rs`.

Interface:
```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemberState { #[default] Active, Paused }
pub struct Member { pub user, pub role, pub joined_at,
    #[serde(default)] pub state: MemberState,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub paused_at: Option<DateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub paused_by: Option<String> }
pub async fn set_member_state(&self, slug: &str, email: &str, state: MemberState, by: &str) -> Result<Membership>;
/// Strict: Ok(None) = team exists and the person is not a member; Err on any read failure.
pub async fn membership(&self, slug: &str, email: &str) -> Result<Option<MemberState>, MembershipErr>; // MembershipErr::{NoSuchTeam, Read(String)}
```
`set_member_state` mirrors `set_role`: pausing the last ACTIVE owner → `Membership::LastOwner`,
enforced in the Mongo filter (`$elemMatch` another active owner) and the Memory branch alike.
`slugs_for(email)` omits teams where the person's state is `Paused` (this is what `teams_for`,
`member_teams` and `may_act` read). A missing `state` reads as `Active`.

Tests (Memory, plus the Mongo tests the file already runs when its env is set):
- `a_member_row_without_state_reads_active`.
- `set_member_state_round_trips_and_stamps_paused_at_and_by`.
- `the_last_active_owner_cannot_be_paused`.
- `slugs_for_omits_a_paused_team`.
- `membership_distinguishes_no_team_not_member_and_paused`.

Run: `cargo test -p kloudlite-pulls directory`.
Commit: `Add a paused state to team members`

## Task 2 — pause and unpause routes

Files: `crates/api/src/teams.rs`, `crates/api/src/lib.rs` (routes).

`POST /api/teams/{slug}/members/{email}/pause` and `/unpause`. Reach = `remove_member`'s
`may_grant(role, target)`; pausing yourself → 403 `you cannot pause yourself`; `LastOwner` → 409
`a team must keep at least one active owner`. On success: `api.membership.forget(&email, &slug)`,
`spawn_keys_changed(&api, &email)` (as `remove_member` does), log `member.paused`/`member.unpaused
{team, member, by}`. Superadmin reach: the same two routes accept a `superadmin` session claim
regardless of team role (check how `team_for` treats superadmin; add the arm there only if absent).

Tests (the file's existing router tests):
- `an_admin_pauses_a_member_and_an_owner_pauses_an_admin`.
- `a_member_cannot_pause_and_nobody_pauses_themself`.
- `pausing_forgets_the_membership_cache` — a `may_act` read after pause is false with no 60 s wait.
- `unpause_restores_membership`.

Run: `cargo test -p kloudlite-api teams`.
Commit: `Let team admins pause and unpause members`

## Task 3 — workspaces `Directory::membership`

Files: `crates/workspaces/src/api/state.rs` (trait), `bins/api/src/main.rs` (`Dir` impl), the test
stubs that implement the trait (`grep -rn "impl Directory for" crates/workspaces/tests` — default
method means none need editing).

Interface:
```rust
pub enum Judged { TeamGone, NotMember, Member(MemberState) }
/// Default Err("unsupported") so every stub stays keep-biased.
async fn membership(&self, team: &str, user: &str) -> Result<Judged, String>;
```
`Dir` resolves a handle to an email with `email_of` (the strict one, NOT `email_or_closed`) and maps
Task 1's `membership`.

Tests: `bins/api` unit with a Memory directory: gone team, non-member, paused, active; unknown
handle is `Err`, never `NotMember`.
Run: `cargo test -p kloudlite-api-bin`.
Commit: `Ask the directory strictly whether a person is in a team`

## Task 4 — `BenchAccess::Paused`, ReadOnly retired

Files: `crates/workspaces/src/crd/bench.rs`, `crates/workspaces/src/k8s/bench.rs`,
`crates/workspaces/src/api/bench.rs` (`Standing`, `my_bench`, `ensure_access`),
`crates/workspaces/tests/api_bench.rs`, CRD yaml if generated (`crates/workspaces/tests/crd_yaml.rs`
tells you).

```rust
pub enum BenchAccess { #[default] Full, #[serde(alias = "ReadOnly")] Paused }
```
`bench_pod`: no `--read-only` arm; a `Paused` bench's controller never wants a pod
(`crd::bench_wants_pod` returns false for Paused). `my_bench`: a non-member answers 404
(`no_team()`), `Standing::Departed` deleted; a PAUSED member (Task 3 `Judged::Member(Paused)`)
answers 403 `your access to {team} is paused`. `ensure_access` never writes access any more (the
reconcile is its only writer): delete it and its call sites.

Tests:
- `a_stored_readonly_bench_parses_as_paused`.
- `a_paused_bench_wants_no_pod`.
- Replace `a_departed_member_reads_their_own_bench_and_nothing_more` with
  `a_departed_member_gets_404_on_their_old_bench`.
- `a_paused_member_gets_403_on_bench_routes`.

Run: `cargo test -p kloudlite-workspaces --test api_bench && cargo test -p kloudlite-workspaces crd`.
Commit: `Replace the read-only departed bench with a paused one`

## Task 5 — `/v1` refuses a paused member's team

Files: `crates/workspaces/src/api/scope.rs`, `crates/workspaces/tests/api_teams.rs`.

`teams_for` already omits paused teams (Task 1), so `may_act_on`/`may_allocate_for` deny. Add the
sentence: `pub(crate) async fn denial(s, c, owner) -> Response` — 403
`your access to {owner} is paused` when `membership(owner, c.name)` is `Member(Paused)`, else the
existing sentence. Route every 403 that follows a `may_act_on`/`may_allocate_for` false through it
(`grep -rn "may_act_on\|may_allocate_for" crates/workspaces/src/api`).

Tests: `a_paused_member_lists_nothing_and_creates_nothing_in_the_team` (403 with the sentence);
`an_active_member_is_unchanged`.
Run: `cargo test -p kloudlite-workspaces --test api_teams`.
Commit: `Say a paused member's team access is paused`

## Task 6 — `membership::reconcile` judge and grace (dry-run)

Files: new `crates/workspaces/src/api/membership.rs` (`//!` header: the lifecycle table, the
keep-bias rule, the delete order, why a beat and not `remove_member`), `crates/workspaces/src/api/mod.rs`,
`crates/core/src/settings.rs` + central settings struct (`member_removal_deletes: bool`, default
false, `Mark::Live`, via `LiveSettings`, never `std::env::var`).

Interface:
```rust
pub const MEMBER_REMOVAL_GRACE: Duration = Duration::from_secs(7 * 24 * 3600);
pub const REMOVED_AT: &str = "kloudlite.io/removed-at";
pub const DELETE_NOW: &str = "kloudlite.io/delete-now";
#[derive(Debug, PartialEq)] pub enum Verdict { Keep, Pause, Unpause, Stamp, Clear, Delete }
/// Pure: the decision for one (owner, team) pair.
pub fn decide(judged: &Result<Judged, String>, stamped_at: Option<i64>, delete_now: bool, now: i64, currently_paused: bool) -> Verdict;
pub async fn reconcile(s: &ApiState);           // every pair
pub async fn reconcile_pair(s: &ApiState, owner: &str, team: &str);
```
Pairs: distinct `(spec.owner, spec.team)` from Bench, Workspace (team set) and SpaceEnvironment
LISTs, dropping `team` empty or equal to `owner` (case-insensitive). Any LIST error → skip the beat.
`decide`: `Err` → Keep (one `membership.reconcile.skipped` log per beat, error count and last
error); `Member(Active)` with a stamp → Clear; `Member(Active)` while paused → Unpause;
`Member(Paused)` → Pause (never Stamp); `TeamGone`/`NotMember` without a stamp → Stamp;
with a stamp older than the grace, or `delete_now` → Delete; otherwise Keep.
Stamp writes `REMOVED_AT` (RFC 3339) on the Bench, or on each of the pair's Workspaces when there
is no Bench, plus audit `member.removed.judged {owner, team, reason ∈ {left_or_removed, team_deleted}, delete_at}`.
Clear removes both annotations. In this task Delete only logs `membership.cleanup.would_delete`.
Wire into `keys::run_beat` in place of `pause_departed_benches` and `prune_team_benches`;
`spaces::prune_departed` stays until Task 7.
Audit: `crate::audit::record(os, &AuditEntry{..})` needs the object store; if `ApiState` in the
`user` role holds none, the judged rows go through the history/audit path the admin process owns
(check `admin/audit.rs`), not a second writer.

Tests (`membership.rs` `mod tests`, fake `Directory` + the fake-kube helper `tests/api_bench.rs`
uses):
- `decide` table test covering every row above.
- `a_directory_error_changes_nothing` (no PATCH, no DELETE issued).
- `a_personal_pair_is_never_a_candidate`.
- `a_removed_pair_is_stamped_once_and_the_second_beat_writes_nothing`.
- `a_readd_during_the_grace_clears_the_stamp`.
- `a_paused_member_is_never_stamped`.
- Delete `pause_departed_benches`, `prune_team_benches`, `orphan_benches`, `gone_teams` and move
  their still-meaningful tests (`a_personal_bench_is_never_pruned`, keep-on-error) here.

Run: `cargo test -p kloudlite-workspaces membership`.
Commit: `Judge removed team members on the keys beat without deleting`

## Task 7 — the delete, in order

Files: `crates/workspaces/src/api/membership.rs`, `crates/workspaces/src/api/spaces.rs` (delete
`prune_departed`, move its tests), `crates/workspaces/src/api/keys.rs` (`run_beat`).

On `Verdict::Delete` and `member_removal_deletes == true`, re-judge first (fresh `membership`
call; anything but TeamGone/NotMember → Keep), then, each step "delete if present", 404 = done,
409 = stop this pair until next beat, other error = stop and log `membership.cleanup.failed`:
1. Bench, uid + resourceVersion precondition from the object judged. The `kloudlite.io/bench-folder`
   finalizer (Task 11) keeps it in `Terminating` until an agent has removed
   `{pool}/homes/.benches/{team}/{owner}`; `membership::reconcile` treats a `DELETE` accepted (200,
   `deletionTimestamp` set) the same as 404 here — it moves on to the next step without waiting for
   the finalizer to clear, since access (the pod, the gateway's `resolve_bench`, `tool-token`) is
   already gone the moment `deletionTimestamp` is set. The folder itself is deleted independently by
   the agent, not by this beat.
2. Every Workspace with `spec.owner == owner && spec.team == team`, each with its own precondition.
   `WORKTREE_FINALIZER` does the rest: worktree and sync points go, the Volume is detached and kept
   when a pushed Snapshot remains (decision 11). The reconcile deletes NO Snapshot, NO Volume and NO
   Environment; intercepts naming the workspace release on their own.
3. The pair's `SpaceEnvironment`.
4. Keys: nothing to write — `project_all` (Task 1's `slugs_for`) already drops them;
   `prune_namespaces` removes the `wt-` namespace when empty.
One audit row per deleted object `member.removed.cleanup {owner, team, kind, name, reason}`, log
`membership.cleanup.deleted`. The bench folder is not one of these rows — Task 11's agent logs
`bench.folder.collected {team, owner}` on its own when the finalizer clears.

Tests:
- `deletes_run_bench_then_workspaces_then_space_choice` (order of fake-kube DELETEs).
- `no_snapshot_volume_or_environment_is_ever_deleted` (fake kube refuses any such DELETE path).
- `a_409_stops_the_pair_and_is_not_forced`.
- `with_deletes_off_nothing_is_deleted`.
- `a_rejudge_that_finds_the_member_back_deletes_nothing`.
- `the_beat_after_a_full_cleanup_writes_nothing`.

Run: `cargo test -p kloudlite-workspaces membership && cargo test -p kloudlite-workspaces spaces`.
Commit: `Delete a removed member's bench, workspaces and space choice after the grace`

## Task 8 — pause stops, unpause starts nothing

Files: `crates/workspaces/src/api/membership.rs`.

`Verdict::Pause`: merge-patch the Bench `spec.access: Paused` and `desiredState: Stopped`; set
`desiredState: Stopped` on each of the pair's Workspaces through the same `set_desired` the stop
routes use (so the stop sync point is cut). Delete the `bench-tool` Secret (best effort). Idempotent:
skip objects already in that state. `Verdict::Unpause`: `access: Full` only. Both run regardless of
`member_removal_deletes` (pause deletes no data). Log `member.pause.applied` / `member.unpause.applied`.

Tests: `pause_stops_bench_and_team_workspaces_and_marks_access`; `pause_twice_writes_nothing`;
`unpause_sets_full_and_starts_nothing`; `a_personal_workspace_of_a_paused_member_is_untouched`.
Run: `cargo test -p kloudlite-workspaces membership`.
Commit: `Stop a paused member's bench and team workspaces`

## Task 9 — delete-now and immediate propagation

Files: `crates/workspaces/src/api/membership.rs`, `crates/workspaces/src/api/mod.rs` (routes),
`crates/workspaces/src/api/admin/owners.rs`, `crates/api/src/teams.rs`.

- `POST /v1/teams/{team}/members/{owner}/delete-now` body `{ "person": owner, "team": team }`:
  caller's `team_role(team) >= Admin`; body must repeat both names exactly (400 otherwise); pair
  must be stamped (409 `not pending removal`). Writes `DELETE_NOW` on the stamped object(s), then
  `reconcile_pair`. Audit `member.removed.delete_now {owner, team, by}`.
- Same function behind the superadmin router in `admin/owners.rs`, plus
  `GET /admin/owners/removals` listing stamped pairs with `delete_at`.
- `GET /v1/teams/{team}/removals` (team admin) → `[{owner, delete_at}]` for the members table.
- `POST /v1/internal/membership/{team}/{owner}` → `reconcile_pair`, authenticated the way the other
  `/v1/internal/*` routes are. The directory's pause, unpause and remove routes call it
  fire-and-forget if they have the api base configured; the beat stays the backstop. If the
  directory binary has no api address today, skip the call and note the 300 s propagation in the
  web copy instead — do not add a new config just for this.
- Classify the new `/v1` routes in `NOT_BENCH_TOOL_ROUTES` (credential plan Task 2).

Tests: `delete_now_requires_admin_and_both_names`; `delete_now_refuses_an_unstamped_pair`;
`delete_now_runs_the_delete_without_waiting_the_grace`; `removals_lists_stamped_pairs_only`.
Run: `cargo test -p kloudlite-workspaces membership && cargo test -p kloudlite-workspaces --test api_admin_owners`.
Commit: `Let an admin delete a removed member's data now`

## Task 10 — gateway refuses paused benches and workspaces

Files: `bins/gateway/src/resolve.rs`.

`resolve_bench`: after the GET, `bench.spec.access == Paused` → `(FORBIDDEN, "access paused")`.
`resolve`: when `ws.spec.team` is set and differs from the owner, GET
`Bench/{crd::bench_id(owner, team)}`; `Paused` → 403; a 404 Bench is allowed (a member may never
have made one), any other error → 409 (retryable, keep-biased toward refusing a tunnel, not data).
Check the tunnel returns 403 before the upgrade.

Tests (the file's fake-kube tests, add if absent): `a_paused_bench_is_403`;
`a_workspace_whose_team_bench_is_paused_is_403`; `a_workspace_with_no_bench_resolves`.
Run: `cargo test -p kloudlite-gateway resolve`.
Commit: `Refuse gateway tunnels to a paused member's bench and workspaces`

## Task 11 — a Bench finalizer marks its folder for deletion, an agent clears it

Files: `crates/workspaces/src/crd/bench.rs` (the finalizer constant),
`crates/workspaces/src/api/bench.rs` (`create_bench` sets it), `bins/agent/src/controller/workspace/bench.rs`
(new; the Bench reconcile — check `grep -rn "impl.*Controller\|reconcile" bins/agent/src/controller/workspace/`
for where Bench is watched today and whether this belongs beside it), `bins/agent/src/controller/workspace/home.rs`
(the delete function, and drop the `ponytail:` note it replaces).

Why a finalizer and not a janitor sweep: the cluster controller (`bins/controller`, see CLAUDE.md
"Cluster controller") is the one process that could hold a cluster-wide mark, but it has no
kubeconfig for mounting the homes share and no disk of its own — an agent is the only process that
mounts `{pool}/homes`. So the mark has to live on the object an agent already watches, and a
finalizer on the Bench itself is that mark: it names exactly the (team, owner) pair whose folder
must go, needs no separate CRD field or annotation contract, and reuses the same
delete-blocks-on-finalizer shape the codebase already uses for `WORKTREE_FINALIZER`
(`cleanup_parent`) on Workspace/Environment. No orphan inference, no `.removed` marker file, no
extra grace on top of Task 7's — the Bench is only deleted after the 7-day grace, so a folder is
never marked before that.

```rust
pub const BENCH_FOLDER_FINALIZER: &str = "kloudlite.io/bench-folder";

/// Validates the path is still `{homes root}/.benches/{team}/{owner}` with no symlink component
/// (reuses the segment validation `k8s::bench_folder` already does) before removing it. Ok(true) =
/// removed or already absent; Ok(false) = the path failed validation — keep-biased, caller must not
/// clear the finalizer.
fn delete_bench_folder(pool: &str, export: &str, team: &str, owner: &str) -> Result<bool, String>;
```

- `create_bench` (`/v1`, the only writer of Bench spec) adds `BENCH_FOLDER_FINALIZER` on create, the
  same place `WORKTREE_FINALIZER` is added for a Workspace.
- Every agent's Bench controller reacts to `deletionTimestamp.is_some() && finalizers contains
  BENCH_FOLDER_FINALIZER`: tear down the pod/Secret as it does today, call `delete_bench_folder`,
  and on `Ok(true)` patch the finalizer off with a resourceVersion precondition (the same
  optimistic-concurrency patch `cleanup_parent` uses). On `Ok(false)` (validation refused the path)
  or an `Err` (the share unreachable, an IO error), do NOT clear the finalizer — log
  `bench.folder.delete.failed {team, owner, err}` (alertable: a Bench stuck in `Terminating` past a
  couple of beats is this) and let the next reconcile retry.
- **Exactly one agent needs to act, and a second one is a harmless no-op by construction, not by a
  claim**: `delete_bench_folder`'s `remove_dir_all` is idempotent (a second caller finds the
  directory already gone and returns `Ok(true)`), and the finalizer-removing patch is a
  resourceVersion CAS — the second agent's patch 409s, which it treats exactly like the analogous
  race in `App::election_tick`/`cleanup_parent`: log at debug and stop, no retry storm, because the
  next watch event already shows the finalizer gone. No node claim, no annotation, no extra RBAC
  beyond the `benches` watch/patch the agent already holds.

Tests (tempdir, reusing the fixture `ensure_bench_folder`'s test already sets up):
- `a_bench_folder_is_deleted_when_the_finalizer_reconcile_runs`.
- `a_second_delete_of_an_already_gone_folder_is_ok`.
- `an_escaping_or_symlinked_path_refuses_and_the_finalizer_stays`.
- `a_share_read_error_refuses_and_the_finalizer_stays`.
Run: `cargo test -p kloudlite-agent bench && cargo test -p kloudlite-workspaces crd`.
Commit: `Delete a bench's folder through its own finalizer`

### One-time cleanup for benches already deleted

Files: a short one-off in `bins/agent` (or a `kl-connect`-style admin script if one-off tooling
already lives outside the binaries — check `deploy/` first) run once, by hand, against today's 4
team benches that were deleted before this finalizer existed and so left their folders behind (the
`ponytail:` note's exact complaint).

- List every `{team, owner}` currently under `{pool}/homes/.benches/*/*` on the share.
- Take a **fresh** `Bench` LIST from the API at run time (not a cache/reflector) and keep only pairs
  with no matching Bench.
- For each: run the same validated `delete_bench_folder`, log `bench.folder.collected {team, owner}`,
  and record the list of what was removed in the ship notes (the audit trail for this one-off).
- This step runs once, is not wired into any beat, and is deleted from the codebase after use if it
  was added as a throwaway binary rather than a `kl-connect` subcommand.
Commit: `Collect bench folders left by benches deleted before the finalizer`

## Task 12 — web: remove dialog, pause controls, pending removals

Files: `web/apps/web/src/components/app/team-settings.tsx`,
`web/apps/web/src/app/(shell)/[owner]/(org)/settings/actions.ts`, `web/apps/web/src/lib/api/`
(the teams module), sibling `*.test.ts`.

- Remove dialog text: "Their bench and workspaces in {team} will be deleted on {date}. Pushed
  snapshots, repositories, images and environments stay with the team." (`date` = now + 7 days,
  `lib/time.ts`).
- Members table: Pause / Unpause per row (shown where `may_grant`), state chip `paused`.
- Pending removals below the table from `GET /v1/teams/{team}/removals`: "removing — data deleted
  at {time}" with a "Delete now" action whose confirm makes the admin type the person's handle.
Copy `settings/` destructive-action siblings; tokens, `--radius: 0`.
Tests: action unit tests for pause/unpause/delete-now request shapes; dialog copy test.
Run: `cd web && bun run lint && bun run typecheck && bun run test`.
Commit: `Show pause, unpause and pending removals on the team page`

## Task 13 — superadmin Owners: pending removals

Files: `web/apps/web/src/app/(shell)/superadmin/owners/…` owners/, `lib/fixtures/superadmin.ts`.
A `Section` "Pending removals" from `GET /admin/owners/removals`, with delete-now behind the same
typed confirmation. Fixture rows so `scripts/superadmin-screens.mjs` renders it offline.
Run: `cd web && bun run typecheck && bun run test`.
Commit: `List pending member removals in the superadmin console`

## Task 14 — SLO `team.member.paused`

Files: `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`, `bins/slo/src/stages/` (the teams
experience module, `experience_teams/`), `bins/slo/src/suite.rs` (group).

Row (Hourly, "14 · Experience", "Teams"): "A paused member's tool token, team `/v1` and bench tunnel
are refused and their bench is stopped within one beat; after unpause and start the bench folder's
canary is still there", `bound(420_000)`.
Journey: second probe member writes a canary file in their bench, pause via the directory route,
poll ≤ 360 s for: tool token call 401, `GET /v1/workspaces?team=` 403 with the paused sentence,
gateway bench tunnel 403, Bench `desiredState: Stopped`. Unpause, start, read the canary.
Tests: catalogue/slo.md equality; group assignment unit.
Run: `cargo test -p kloudlite-workspaces slo && cargo test -p kloudlite-slo-bin`.
Commit: `Probe pausing a team member`

## Task 15 — SLO `team.member.removed.cleanup` and `team.member.removed.dir_down`

Files: same as Task 14, drills suite.

- `team.member.removed.cleanup` (drills): member with a bench (canary), a team workspace with one
  PUSH, remove them, wait for the stamp, call delete-now, then within two beats: Bench, team
  Workspace, transient sync points and SpaceEnvironment gone, and the Bench's
  `kloudlite.io/bench-folder` finalizer cleared with `{pool}/homes/.benches/{team}/{owner}` gone from
  the share (Task 11 — an agent clears it, not this beat); the pushed Snapshot and its detached
  Volume STILL present (decision 11); `member.removed.*` audit rows exist; re-adding the person
  finds no bench. Teardown deletes the kept snapshot by its `run-{id}` name.
- `team.member.removed.dir_down` (drills): with the api's directory address pointed at a black
  hole for one beat (the drills suite's existing fault hook; if none exists, mark the id skipped with
  the reason and file it — do not build a fault injector in this task), a removed pair's objects all
  survive and `membership.reconcile.skipped` is logged.
Run: `cargo test -p kloudlite-workspaces slo && cargo test -p kloudlite-slo-bin`.
Commit: `Probe removed member cleanup and its directory-down keep`

## Task 16 — enable deletes and document

Files: `CLAUDE.md` ("Workspaces and environments": a paragraph on the lifecycle table, the
keep-biased beat, the 7-day grace, what is team-owned and never deleted, pause = stop + refuse at
`/v1`, gateway and tool token), central/cluster settings value flip (admin settings PUT, recorded in
the ship notes, not a code default change).

Precondition: the owner has read a week of `member.removed.judged` rows and said go. Flip
`member_removal_deletes` to true in the region's settings; one beat later confirm
`membership.cleanup.deleted` rows match the reviewed list and the drills suite passes
Task 15's ids on the carrying build.
Commit: `Document team member removal and pause`
