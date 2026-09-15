# Bench tool credential — design

Status: approved decisions · owner review 2026-09-15 · builds on `2026-09-14-desktop-login-design.md`

## Problem

Two different network paths are involved, and only one of them exists today.

- **Inbound: desktop → bench.** The desktop reaches the bench through the gateway tunnel. The
  gateway checks a `bench-session` token and dials the pod (`bins/gateway/src/tunnel.rs`,
  `resolve_bench` in `bins/gateway/src/resolve.rs:57`). The gateway only pipes bytes. It holds no
  credential and cannot call the api on anyone's behalf.
- **Outbound: bench pod → platform api.** The bench's `kl_*` tools (`harness/pi/kloudlite.ts`, also
  `call` in `harness/pi/workspace-tools.ts`) call `/v1/workspaces`, `/v1/environments`,
  `/v1/regions`, `/v1/quota`, `/v1/volumes`, `/v1/builders/me` and `/v1/workspaces/{id}/tools`.
  These calls leave the pod and go to the public api host. The gateway tunnel does not carry them,
  so the desktop's login is of no use to them.

The pod holds no platform credential, so today a person runs `/kl-login` inside the bench. That is a
second device-code login, and its 30-day CLI token is saved to `~/.config/kl-connect/config.json`.
On a bench that path is the NFS home `/home/kl` (`k8s/bench.rs` `home_volume`), the same home every
workspace of that owner mounts. The result is a full, unscoped, month-long login that anything
running in any of their workspaces can read.

The desktop has already authorized the session, so a second login is unnecessary.
`POST /v1/bench/session` is already called with the desktop's CLI token (which carries a `jti`)
and already proves who is using which bench in which team.

## Requirements

1. The desktop's CLI token never leaves the laptop's main process.
2. The platform mints the pod's credential, scoped to the bench owner's handle plus the bench's
   team. It is accepted only on the tool routes above, never on `/v1/cli/*`, `/v1/keys*`,
   `/v1/bench/*`, `/v1/internal/*` or admin.
3. The credential lives 15 minutes and is renewed every 5 minutes, but only while a desktop is
   connected and the bench is running. It dies when the parent login is revoked (the parent `jti`
   is checked on every use through `cli_token_live`), when the bench is stopped, when the person
   signs out, and when the member is paused or removed.
4. It never appears in logs, in pod-spec env, or in a file another user can read. Every tool call
   reads it fresh.
5. It fails closed. With no credential or an expired one, the tool answers
   `sign in on the Kloudlite desktop app` and never starts a device code.
6. `/kl-login` is deleted, and every login it already created is revoked and removed.
7. The bench is the member's personal workspace inside the team. It lasts only as long as the
   membership (see "Membership lifecycle").

## Decisions (owner, 2026-09-15)

1. **TTL.** After the laptop disconnects, tools keep working for 15 minutes: a 15-minute token
   renewed every 5 minutes while the desktop is connected. Approach A.
2. **Old logins.** The platform revokes every existing `(bench)` CLI token and deletes
   `~/.config/kl-connect/config.json` from bench homes. This is rollout step 6.
3. **Scope.** The token acts for the bench's team and for the person's own handle.
4. **Bench = personal workspace in the team.** When a person leaves a team, everything they own
   in that team is deleted, data included. A separate pause state keeps the data but removes
   access. See "Membership lifecycle".
5. **API address.** The pod calls the platform api at the public host, not through the gateway.
   The ingress allow-list must carry every tool family. `deploy/kloudlite-web.yaml:253` already
   lists `environments|regions|quota|volumes|me` on this branch. The rollout step verifies that the
   pin being rolled carries that line, and a tool call from a pod must never get the HTML page
   back.

## Approach A — derived `bench-tool` JWT, projected into a Secret

**Token.** Add a new kind to `crates/core/src/jwt.rs` next to `bench-session`:

```
BenchToolClaims { sub: handle, team, bench, parent: cli jti, jti, iat, exp, typ: "bench-tool" }
BENCH_TOOL_TTL_SECS = 900
mint_bench_tool(handle, team, bench, parent) / verify_bench_tool(token)
```

`verify` and `verify_any_user` already refuse every other `typ` through `verify_typed`. The
directory tier (`crates/api`), the admin process and the registry therefore reject this kind with
no change. The only place that accepts it is one new arm in `caller`.

**Mint and renew.** Add `POST /v1/bench/tool-token?team=` in `api/bench.rs`. It goes through
`my_bench`, so the membership and 404 rules stay as they are. It refuses:

- a caller with no `jti` (a web session cookie): 403;
- a `bench-tool` caller: 401, so a pod cannot extend its own token;
- a bench whose `desiredState` is `Stopped`: 409;
- a caller who is not an active member (`Standing` other than `Member`, which includes paused): 403.

To support this, `caller` returns the `jti` it already computes as `Caller.parent`. The route
server-side-applies the Secret `bench-tool` (key `token`, annotation `kloudlite.io/exp`) into
`ws_namespace(owner, team)` and answers 204 with no body. The desktop calls it:

- once in Connect, right after `ensureBench`;
- every 5 minutes while the app is signed in and connected to that bench.

The gap between the 5-minute renew and the 15-minute TTL absorbs the kubelet's Secret-volume sync,
which takes about 60–90 s. Renewal stops when the desktop quits, disconnects or signs out. The pod
already exits after `benchIdleSecs` once no WebSocket is open.

**Delivery.** `k8s::bench_pod` adds a Secret volume `bench-tool` (`optional: true`, mode 0444),
mounted read-only at `/etc/kloudlite/bench-tool`. Only file paths go into env:
`KL_TOOL_TOKEN_FILE=/etc/kloudlite/bench-tool/token` and `KL_API_URL` (the public api host). This
is the same projection `user-key` uses. Rotating the credential means rewriting the Secret, and the
kubelet swaps the file atomically through a symlink. Mode 0444 is justified the same way as
`user_key_volume`: the kubelet owns the file as root, the process runs as `kl`, and the pod belongs
to one person.

The namespace belongs to one (owner, team) pair (`crd::ws_namespace`), so no other user's pod can
mount the Secret. Workspace pods in that namespace do not mount it either, which is why its name is
separate from `user-key`. The Secret never goes on the shared home. `kloudlite.ts` reads the file
on every call (a few hundred bytes), so a rotation is picked up with no watcher.

**Use.** `caller` gets one extra arm for `typ == "bench-tool"`. Every request runs these checks:

1. **Audience.** Method and path must match `BENCH_TOOL_ROUTES`, a single table in `api/mod.rs`
   that covers the seven route families above, including their `start`, `stop`, `push`, `clone`,
   `attach`, `detach`, `intercepts` and `packages` subroutes. Anything else gets 401. A test holds
   the table against the router, the way `every_browse_route_is_routable` does, so every new `/v1`
   route has to be classified.
2. **Parent.** `cli_token_live(parent)` must be true. This uses the existing 30 s positive cache, so
   a revoked desktop login stops working within 30 s.
3. **Bench.** A GET on `Bench/{bench}` must show `spec.owner == sub`, `spec.team == team`,
   `desiredState != Stopped` and `access == Full`. Otherwise the answer is 401. This makes a stop
   take effect immediately, even while a copy of the token is still in memory. Paused and deleted
   benches fail this check.
4. **Scope.** `Caller { name: sub, superadmin: false, scope: Some(team) }`. The new `scope` field is
   read in `api/scope.rs`: `may_act_on` and `may_allocate_for` refuse any owner other than `sub` and
   `team` with 403. A tool call that names another team fails even if the person belongs to it.

**Revoke.**

- `stop_bench` deletes the Secret, and check 3 already refuses the token.
- On sign-out, the desktop sends `DELETE /v1/bench/tool-token?team=`, best effort (it deletes the
  Secret), and then `DELETE /v1/cli/tokens/{jti}` (existing). Check 2 refuses the token within 30 s.
- Pause sets `access: Paused` and removal deletes the Bench. Check 3 refuses the token either way.
- With no desktop live, the token expires within 15 minutes.

## Approach B — the desktop answers tool calls over the session (rejected)

When the pod needs `/v1`, it sends an `api` request over the BenchClient WebSocket that is already
open. The desktop main process checks the request against the same route table and replays it with
its own CLI token.

| | A: derived token in a Secret | B: desktop proxies |
|---|---|---|
| New server surface | one JWT kind, one route, a `caller` arm, one pod volume | none |
| Tools with no desktop connected | work until TTL | fail |
| Audience enforced by | the api (server side) | the desktop (client side; a modified app bypasses it) |
| Revocation | ≤30 s on parent, immediate on stop | immediate |
| Latency | pod → api | pod → gateway → laptop → api → back |
| Multiple devices on one bench | n/a | must choose which device answers; racy |

The bench runs turns on the server side and is shared by every device. A credential that exists
only while one particular laptop is connected contradicts that design.

## Membership lifecycle

**Rule.** A bench is the member's personal workspace inside the team. It exists only while the
person is a member. A member can be in one of three states:

| State | Access | Data |
|---|---|---|
| Active | full | kept |
| Paused | none | kept |
| Removed (removed, left, or team deleted) | none | member's bench, workspaces and choices deleted after a 7-day grace; team-owned data kept |

### What exists today

- `my_bench` (`crates/workspaces/src/api/bench.rs:115`) answers `Standing::Departed` for a person
  who is no longer a member but still has a Bench. `ensure_access` (`:141`) sets
  `spec.access = ReadOnly` for that person, and `create_bench` (`:232`) 404s them.
- `readonly_departed_benches` (`crates/workspaces/src/api/keys.rs:161`) runs on the keys beat
  (`run_beat`, `KEYS_RESYNC_SECS` = 300 s). It flips a departed member's Full bench to ReadOnly and
  keeps the Bench. It relies on `teams_for`, which fails closed to an empty list, so the demotion
  also happens when the directory is down.
- `prune_team_benches` (`keys.rs:204`) deletes a bench only when the directory ANSWERS that the
  team is gone (`gone_teams`), and pins the delete to the judged uid and resourceVersion
  (commit 31ccb07f). A departed member of a team that still exists is not touched.
- `spaces::prune_departed` (`crates/workspaces/src/api/spaces.rs:208`) deletes a departed member's
  `SpaceEnvironment`. It is keep-biased: `member_teams` errors mean nothing is pruned.
- Nothing deletes the bench folder `{pool}/homes/.benches/{team}/{owner}`. See the `ponytail:`
  note in `ensure_bench_folder` (`bins/agent/src/controller/workspace/home.rs:52`).
- Nothing deletes a departed member's team Workspaces (namespace `wt-{owner}-…`). `prune_namespaces`
  only removes a `wt-` namespace once no Workspace resolves to it.
- The directory's `Member` (`crates/pulls/src/directory/mod.rs:34`) is `{user, role, joined_at}`.
  It has no state field. `remove_member` (`crates/pulls/src/directory/teams.rs:442`, route
  `crates/api/src/teams.rs:864`) `$pull`s the row and does nothing else.

### What changes

**ReadOnly departed benches go away.** `Standing::Departed`, `BenchAccess::ReadOnly` and
`readonly_departed_benches` are deleted. `my_bench` answers 404 for a non-member, exactly as it
does for a person who never had a bench. A stored `ReadOnly` value still parses, is read as
`Paused` until the removal reconcile collects it, and is never written again.

**Removal reconcile.** One beat function, `api::membership::reconcile`, replaces
`readonly_departed_benches`, `prune_team_benches` and `spaces::prune_departed` on the keys beat
(`run_beat`, api `user` role). The controller-owns-cleanup rule applies: `remove_member` and
`delete_team` live in the directory binary, which has no kubeconfig, and a beat also heals a
removal that happened while the api was down.

For each distinct (owner, team) pair with `team != owner`, taken from Bench, Workspace,
SpaceEnvironment and the bench folders on the share:

1. **Judge (keep-biased).** Delete only when the directory ANSWERS either "team gone"
   (`bench_team` → `Ok(None)`) or "team exists and this person is not in it". The second answer
   needs a new strict `Directory::membership(team, user) -> Result<Option<MemberState>, String>`.
   `teams_for` must not be used for this, because it answers an empty list on failure. Any error,
   timeout or `Source::Unavailable` keeps everything, logs `membership.reconcile.skipped` once per
   beat, and is retried on the next beat. A paused member is a member and is never removed.
2. **Grace.** The first beat that judges a pair removed stamps `kloudlite.io/removed-at` on the
   Bench (or, if there is no Bench, on each of that pair's Workspaces). Deletion starts only on a
   beat at least `MEMBER_REMOVAL_GRACE` after that stamp, and it re-judges first. A re-add during
   the grace clears the stamp and nothing is lost. Decided: see decision 8.
3. **Delete, in this order.** Access goes first and bytes go last, so a failure partway leaves
   data behind rather than access:
   1. The Bench, with a uid/resourceVersion precondition as `prune_team_benches` already does.
      Its pod and the `bench-tool` Secret go with the namespace objects. The gateway's
      `resolve_bench` 404s from this point.
   2. Every Workspace with `spec.owner == owner` and `spec.team == team`. This runs the existing
      `WORKTREE_FINALIZER` (`cleanup_parent`), which drops the worktree and sync points and detaches
      the Volume only if a snapshot remains.
   3. Pushed snapshots are TEAM-OWNED and never deleted (decision 11). `cleanup_parent` already
      detaches the Volume when a non-transient `Snapshot` remains, so the volume survives detached
      with its snapshots for the team to restore from; with none left, Kubernetes GC and
      `retire_pass` collect it. Transient sync points go with the Workspace as they always do.
   4. The `SpaceEnvironment` for (owner, team).
   5. Keys projection: `project_all` stops listing the person's keys for that team's namespace on
      its next write. `prune_namespaces` removes the `wt-` namespace (and `user-key`) once it holds
      no Workspace and no pod.
   6. The bench folder is not a separate step here: `create_bench` stamps every Bench with a
      `kloudlite.io/bench-folder` finalizer at creation, so step 1's Bench `DELETE` sets
      `deletionTimestamp` but the object stays `Terminating` until an agent has removed
      `{pool}/homes/.benches/{team}/{owner}` and cleared the finalizer. The api cannot reach the
      share; every agent already mounts it, and any agent whose controller sees the
      `deletionTimestamp` may act — a second one is a harmless no-op (`remove_dir_all` on an already
      gone directory, and the finalizer-clearing patch is a resourceVersion CAS the loser 409s on).
      This is what clears the `ponytail:` note in `ensure_bench_folder`. The mark is owned by the
      cluster controller in the sense that decides deletion (this reconcile only requests the Bench
      delete); the agent is the only process that can do the host work, so it is the one that clears
      the mark.
   Environments are owned by the team (`spec.owner` = team) and are NOT deleted. No creator is
   recorded on them. Decided: see decision 10.
4. **Idempotent.** Every step is "delete if present". A 404 counts as done and a 409 (precondition)
   means the object changed, so it is re-judged next beat. A pair whose objects are all gone is no
   longer listed. A second beat therefore writes nothing.
5. **Audit.** Each deletion writes one `crate::audit::record` row, `member.removed.cleanup`, with
   `{owner, team, kind, name, reason ∈ {left, removed, team_deleted}}`, plus `member.removed.judged`
   when the grace stamp is set. These rows are also dual-written to history as `admin.<action>`.
   Logs: `membership.cleanup.deleted`, `membership.cleanup.failed`,
   `membership.reconcile.skipped`.
6. **What people see.**
   - The person, in the desktop and web: the team leaves their list, and `/v1/bench` for it 404s.
     While the grace runs, the team page's remove dialog says "their bench and workspaces in this team
     will be deleted on {date}; pushed snapshots stay with the team".
   - The admin: the members table shows "removing — data deleted at {time}". Superadmin Owners
     lists pending removals. The Audit area shows the rows above.

### Pause and unpause

**Directory.** Add `state: active | paused` to `Member` (a missing field reads as active), plus
`paused_at` and `paused_by`. `Directory::set_member_state(slug, email, state)` in
`crates/pulls/src/directory/teams.rs` gets the same shape as `set_role`. Routes:
`POST /api/teams/{slug}/members/{email}/pause` and `/unpause` in `crates/api/src/teams.rs`.

**Who can pause.** A team admin can pause members and admins, and a team owner can pause anyone.
This is the same reach as `may_grant` in `remove_member`. A superadmin can pause anyone through the
admin router, and that writes an audit row. Nobody can pause themselves, and the last active owner
cannot be paused (the same rule as `LastOwner`).

**Effect.** A paused member is not a member for access purposes and IS a member for data purposes:

- `teams_for` / `member_teams` / `may_act` omit teams where the person is paused. `/v1` on that team
  answers 403 `your access to {team} is paused`, and a push to the team's git repos and registry is
  refused.
- The `bench-tool` token fails check 3 (the Bench is `access: Paused`), and `tool-token` 403s.
- The gateway gains one read. Before it dials, `resolve_bench` and `resolve` refuse a Bench with
  `access == Paused`, or a Workspace whose Bench pair is paused, with 403. The gateway has no
  directory, so it reads the CR field, which `membership::reconcile` writes on every beat from the
  directory's answer.
- Workspace SSH and the tool server: the keys projection drops the person's keys from that team's
  `OwnerKeys` on the next write, and `/v1/workspaces/{id}/tools` 403s through `may_act_on`.
- Removal reconcile: a paused member is never judged removed, and the data is kept indefinitely.

**Running pods.** Recommended: pause STOPS them. The reconcile sets `desiredState: Stopped` on the
member's Bench and team Workspaces in that team, which cuts the usual stop sync point, so nothing
keeps running (or spending quota) on behalf of someone who has no access. Unpause restores access
only and starts nothing; the person starts things again. Decided: see decision 9.

**Propagation.** Pause takes effect in these stages:
- immediately on the pause route itself;
- within 60 s for `may_act` (git over ssh and http, registry), through its `MEMBERSHIP_TTL` cache
  — the same lag as a removal;
- within the directory cache TTL for `teams_for` on the api;
- within one keys beat (300 s) for `Bench.spec.access`, the gateway refusal, the stop and the keys
  projection.

For immediate effect, `pause` calls the api's existing `POST /v1/internal` resync path to run
`membership::reconcile` for that one pair. The beat remains the backstop. Unpause propagates on
the same schedule.

## Pod side (`harness/pi/kloudlite.ts`)

- Remove `load`, `save`, `dir`, `file`, `DEFAULT_API` and the `Config` type. Also remove the
  imports of `spawn` and `os` if nothing else uses them.
- `call` reads `KL_TOOL_TOKEN_FILE` and `KL_API_URL` on every call:
  - a missing file, an empty file or unset env throws `sign in on the Kloudlite desktop app`;
  - a 401 gives the same message plus `(your desktop session ended or the bench was stopped)`;
  - a 403 passes the server's own sentence through, which covers paused access and scope.
- Keep the `redirect: "error"` and HTML-page guards.
- `kl_whoami` decodes the claims without verifying them and returns `{username, team, expires_at}`.
  It never returns the token.

## Removed code paths

- `harness/pi/kloudlite.ts`: the `pi.registerCommand("kl-login", …)` block (lines 163–195), the
  file-config helpers (22–35), the `/kl-login` strings on lines 41 and 160, and the doc comment
  (10–17) that describes the 30-day token.
- `harness/bench/test/workspace-tools.test.ts`: the `KL_CONFIG_DIR` fixture (129–142) becomes a
  `KL_TOOL_TOKEN_FILE` temp file.
- Every mention of `/kl-login`. After the change, `git grep kl-login` is empty.
- `Standing::Departed`, `ensure_access`'s ReadOnly arm, `readonly_departed_benches`,
  `prune_team_benches` and `spaces::prune_departed` are folded into `membership::reconcile`, and
  their tests move with them.
- `POST /v1/cli/code` stays, because the desktop and `kl-connect` still use it.

## Errors

| Case | Answer |
|---|---|
| No Secret yet (before first Connect, or after stop) | tool: sign-in message; no request sent |
| Token expired (desktop gone >15 min) | api 401 → tool: sign-in message |
| Parent revoked | api 401 within 30 s |
| Directory unreachable (`is_live` false) | 401; fails closed, as CLI tokens do today |
| Bench stopped, deleted or paused | 401 |
| Route outside `BENCH_TOOL_ROUTES` | 401 |
| Owner or team outside scope | 403 `bench tools act only for {handle} and {team}` |
| Member paused, any `/v1` on that team | 403 `your access to {team} is paused` |
| Gateway tunnel to a paused bench or workspace | 403 before upgrade |
| `tool-token` with a session cookie / bench-tool token | 403 / 401 |
| Secret write fails | 503 to the desktop; the beat retries; the old token works until exp |
| Removal reconcile cannot read the directory | nothing deleted; `membership.reconcile.skipped` |

## Audit and logging

- `bench.tool_token.written`: `owner`, `team`, `jti8`, `parent8` (first 8 hex characters), `exp`.
- `bench.tool.refused`: `owner`, `jti8`, `reason ∈ {audience, parent, bench, scope, expired, paused}`.
- The token, the Secret body and full `jti`s are never logged. `kube_err` on the Secret patch must not
  echo the request body; a test checks this.
- Every `/v1` write the tool makes is logged as `http.write` with the caller handle, plus
  `via=bench-tool`.
- `member.paused` / `member.unpaused` audit rows `{team, member, by}`. For removals, see
  "Membership lifecycle".

## Tests

**Unit.**

- `jwt.rs`:
  - mint and verify round trip;
  - `verify` and `verify_any_user` refuse `bench-tool`;
  - `verify_bench_tool` refuses `bench-session` and `cli`.
- `api`: the route-table test covers every `/v1` route. `tests/api_bench.rs` covers:
  - the pod token works on `GET /v1/workspaces`;
  - it gets 401 on `/v1/cli/tokens`, `/v1/bench/session`, `/v1/bench/tool-token` and `/v1/keys`;
  - it gets 401 once `is_live` is false, once the bench is stopped, and once the bench is paused;
  - it gets 403 on another team;
  - `tool-token` gets 403 with a session JWT and 403 for a paused member.
- `membership::reconcile` with a fake directory:
  - a directory error deletes nothing;
  - a removed pair inside the grace is only stamped;
  - after the grace, the delete runs in order (Bench before Workspaces before
    SpaceEnvironment);
  - a re-add during the grace clears the stamp;
  - a paused member is never judged removed;
  - a personal pair (`team == owner`) is never a candidate;
  - the second beat writes nothing;
  - a 409 precondition failure is retried and not forced.
- `directory`:
  - `set_member_state` round trip on Memory and Mongo;
  - a missing `state` reads as active;
  - the last active owner cannot be paused;
  - `teams_for` omits paused teams.
- `gateway`: `resolve_bench` and `resolve` refuse `access: Paused` with 403.
- agent: a Bench's finalizer reconcile removes `{pool}/homes/.benches/{team}/{owner}` and clears
  `kloudlite.io/bench-folder` on delete; a second delete of an already-gone folder is `Ok`; a path
  that fails validation (symlink, outside the homes root) refuses and leaves the finalizer, logged.
- `k8s`: `bench_pod` mounts `bench-tool` optional and read-only, with no token in env;
  `workspace_pod` does not mount it.
- `harness`: `node --test` checks that `call` re-reads the file between calls, that a missing file
  gives the exact message with no fetch, and that 401 maps to that message.

**SLO** (`crates/workspaces/src/slo/catalogue.rs` plus `deploy/slo.md`):

- `bench.tool.token` (hourly): the probe's login calls tool-token and then runs `kl_regions`'s fetch
  inside the bench pod. It passes.
- `bench.tool.audience` (hourly): the pod token on `/v1/cli/tokens` and `/v1/bench/session` is
  refused.
- `bench.tool.revoked` (hourly): the probe revokes the parent `jti`, and a pod call returns 401
  within 60 s. After a stop, the next call returns 401 at once.
- `team.member.paused` (hourly): pause the probe's second member.
  - Within one beat: the tool token gets 401, `/v1` on the team gets 403, the gateway bench tunnel
    gets 403, and the bench is `Stopped`.
  - Unpause, then start: access returns and the bench folder's canary file is still there.
- `team.member.removed.cleanup` (drills suite, with the grace overridden short for the probe team):
  remove the member.
  - After grace plus two beats, these are all gone: the Bench, the team Workspaces, the transient
    sync points, the SpaceEnvironment and the bench folder.
  - The audit rows exist, and a re-add of the same person finds nothing.
- `team.member.removed.dir_down` (drills suite): with the directory unreachable, a removed pair's
  objects all survive the beat.

## Rollout order

1. **directory.** Add the `Member.state` field (a missing value reads as active), `set_member_state`,
   and the pause/unpause routes. `teams_for` / `may_act` omit paused teams. This step is inert until
   somebody pauses a member.
2. **api.** Add:
   - the JWT kind, the `caller` arm with the route table and scope, `tool-token`, and the Secret
     delete in `stop_bench`;
   - `membership::reconcile`, first in **dry-run**: it logs and audits `member.removed.judged`
     without deleting anything, for one week. Before enabling deletes, the owner reads the list;
   - the ingress: confirm that the rolled pin of `deploy/kloudlite-web.yaml` carries the line-253
     allow-list, and check that `curl https://{api-host}/v1/regions` answers JSON with no token
     (401 JSON, not HTML).
3. **gateway.** Add the paused refusal in `resolve_bench` / `resolve`.
4. **agent/k8s.** Add the optional `bench-tool` volume in `bench_pod`, and the Bench finalizer
   reconcile that removes the bench folder and clears `kloudlite.io/bench-folder`. This needs no
   separate dry-run flag: it only ever runs once a Bench is actually being deleted, which itself
   stays gated behind `member_removal_deletes` (Task 7) until the owner turns deletes on. A running
   pod is not replaced; its next wake picks up the volume.
5. **desktop.** Call tool-token at Connect and on the 5-minute beat, and send the delete on
   sign-out.
6. **bench image and old-login cleanup.** Ship the new `kloudlite.ts` without `/kl-login`, then run
   one sweep. It is superadmin-only and audited:
   - **Tokens: find.** List the directory's CLI tokens whose device label ends in `(bench)`. That is
     the label `/kl-login` wrote.
   - **Tokens: revoke.** Revoke each one through the existing revoke path, with an audit row
     `bench.login.revoked {owner, jti8}`.
   - **Tokens: verify.** A second listing returns zero, and one sampled revoked token gets 401 on
     `GET /v1/workspaces`.
   - **Files: find and delete.** Every agent's janitor, on one beat, deletes
     `{pool}/homes/{owner}/.config/kl-connect/config.json` only when its JSON `device` label ends
     in `(bench)`. A laptop `kl-connect` config synced into a home is left alone. It logs
     `bench.login.file.deleted {owner}`.
   - **Files: verify.** `find {pool}/homes -path '*/.config/kl-connect/config.json'` on one node,
     with each hit's label checked, shows no `(bench)` file. The share is region-wide, so one node
     is enough per region.
7. **Enable deletes.** Turn off dry-run for `membership::reconcile` and the janitor once the owner
   has reviewed the week's `member.removed.judged` rows. The ReadOnly bench code is removed in the
   same release.

## Decisions (owner, 2026-09-15 15:33 IST)

These replace the open questions that stood here.

8. **Deletion grace.** 7 days after a removal, a leave or a team delete. An admin can "delete now"
   with a confirmation that names the person and the team. The removal dialog states the date.
9. **Pause stops pods.** Pause sets `desiredState: Stopped` on the member's running Bench and team
   Workspaces in that team. Unpause restores access and starts nothing.
10. **Team environments are kept.** They are team-owned. Only the member's intercepts and
    attachments go, with their Workspaces.
11. **Team-owned data is never deleted on member removal**: pushed (non-transient) Snapshots, git
    repos and container images. Removal deletes only the member's Bench and its region-share folder,
    their team Workspaces (with their sync points, transient snapshots and unpushed working data),
    their `SpaceEnvironment` choice, and their per-team key projections. The Volume
    reference-counting rules keep a volume alive and detached while a pushed snapshot remains.
