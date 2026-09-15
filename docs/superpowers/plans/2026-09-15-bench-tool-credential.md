# Bench tool credential — implementation plan

Spec: `docs/superpowers/specs/2026-09-14-bench-tool-credential-design.md` (6297a326), Approach A
only. Membership removal and pause are a separate plan (`2026-09-15-team-removal-and-pause.md`);
this plan keeps check 3 (bench state) written so a `Paused` access value slots in without a change
to the check's shape.

Goal: the bench pod's `kl_*` tools authenticate with a 15-minute `bench-tool` JWT the platform
mints from the desktop's CLI login, projected into a Secret, accepted by `/v1` on the tool routes
only, and `/kl-login` with every login it made is gone.

Rules for every task:
- TDD: write the failing test first, run it red, implement, run it green.
- Rust: `cargo test -p <crate>` for the touched crate, then
  `cargo clippy --workspace --all-targets -- -D warnings` before the commit.
- Harness: `cd harness && node --test bench/test/<file>.test.ts`.
- One commit per task, imperative sentence case subject, no tool attribution.
- Never log a token, a Secret body or a full `jti`; 8-hex prefixes only (`jti8`, `parent8`).

Rollout order (matches spec "Rollout order", steps 2, 4, 5, 6): Tasks 1–6 ship with the api and are
inert (nothing mints a token until the desktop asks). Task 7 ships with the agent. Task 8 with the
desktop. Tasks 9–10 with the bench image. Task 11 is the one-time cleanup, run after Task 10 is on
the fleet. Tasks 12–13 are SLO and go last.

---

## Task 1 — `bench-tool` JWT kind

Files: `crates/core/src/jwt.rs`

Interface (used by Tasks 3, 4):
```rust
pub const BENCH_TOOL_TTL_SECS: u64 = 900;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchToolClaims { pub sub: String, pub team: String, pub bench: String,
    pub parent: String, pub jti: String, pub iat: u64, pub exp: u64, pub typ: String }
impl Jwt {
    pub fn mint_bench_tool(&self, handle: &str, team: &str, bench: &str, parent: &str) -> Result<(String, BenchToolClaims)>;
    pub fn verify_bench_tool(&self, token: &str) -> Result<BenchToolClaims>; // verify_typed(.., "bench-tool")
}
```
Place it beside `mint_bench_session`, same shape.

Tests (in the file's `mod tests`):
- `a_bench_tool_token_round_trips_and_lives_fifteen_minutes` — `exp - iat == 900`, fields echo.
- `a_bench_tool_token_is_not_a_user` — `verify` and `verify_any_user` both `Err`.
- `verify_bench_tool_refuses_bench_session_and_cli` — both `Err`.

Run: `cargo test -p kloudlite-core jwt`.
Commit: `Add the bench-tool token kind`

## Task 2 — `BENCH_TOOL_ROUTES` table held to the router

Files: `crates/workspaces/src/api/mod.rs`

Interface (used by Task 4):
```rust
/// (method, axum path pattern). The pod's audience; everything else refuses a bench-tool caller.
pub(crate) const BENCH_TOOL_ROUTES: &[(&str, &str)] = &[ ... ];
pub(crate) fn bench_tool_route(method: &Method, path: &str) -> bool; // segment match, `{x}` = one non-empty segment
```
Contents: every method/path the tools in `harness/pi/kloudlite.ts` and `workspace-tools.ts` call —
`/v1/workspaces` (GET, POST), `/v1/workspaces/restore`, `/v1/workspaces/{id}` (GET, PATCH, DELETE),
`/v1/workspaces/{id}/{tools|packages/update|clone|push|start|stop|attach|detach}`,
`/v1/environments` family incl. `start|stop|clone|push|restore|restore-in-place|intercepts|intercepts/{service}`,
`/v1/me/environments`, `/v1/me/environments/{team}` (PUT, DELETE), `/v1/regions`, `/v1/quota`,
`/v1/volumes` family (`history`, `refs`, `{name}` DELETE, `snapshots/{snapshot}` DELETE),
`/v1/builders/me`. Not `ssh-session`, not `/v1/bench/*`, `/v1/keys*`, `/v1/cli/*`, `/v1/internal/*`,
`/v1/requests*`, `/v1/quota-requests`.

A second table `NOT_BENCH_TOOL_ROUTES` lists every other `/v1` route pattern. The test mirrors
the existing environments-subroute test near line 120: collect every `.route("/v1…")` path
registered by `router()` (reuse the source-scan helper that test uses) and assert each is in
exactly one of the two tables.

Tests:
- `every_v1_route_is_classified_for_bench_tools` (fails when a route is added unlisted).
- `bench_tool_route_matches_patterns` — `GET /v1/workspaces/w1` true, `POST /v1/workspaces/w1/ssh-session` false,
  `GET /v1/cli/tokens` false, `POST /v1/bench/tool-token` false, `GET /v1/workspaces/a/b/c` false.

Run: `cargo test -p kloudlite-workspaces api::`.
Commit: `Classify every /v1 route for the bench tool audience`

## Task 3 — `Caller.parent` and `Caller.scope`, scope enforced

Files: `crates/workspaces/src/api/state.rs`, `crates/workspaces/src/api/mod.rs` (`caller`),
`crates/workspaces/src/api/scope.rs`, every `Caller { .. }` literal (`grep -rn "Caller {" crates/workspaces`).

Interface:
```rust
pub struct Caller { pub name: String, pub superadmin: bool,
    /// The CLI `jti` this request authenticated with; None for a session cookie or bench-tool.
    pub parent: Option<String>,
    /// Some(team) for a bench-tool caller: acts only for `name` and this team.
    pub scope: Option<String> }
pub(crate) fn in_scope(c: &Caller, owner: &str) -> bool; // scope None → true; else owner == name || owner == team
```
`caller` sets `parent: jti.clone()` for a CLI token. `may_act_on` and `may_allocate_for` return
false first when `!in_scope`. A handler that answers 403 from those keeps its sentence; the listing
handlers that take `?team=`/`?owner=` answer `403 bench tools act only for {handle} and {team}` when
out of scope (one helper `scope_refusal(c)` in `scope.rs`).

Tests (`scope.rs` `mod tests`, and `crates/workspaces/tests/api_bench.rs` later in Task 4):
- `in_scope_is_own_handle_and_team_only`.
- `a_scoped_caller_is_refused_another_team_it_belongs_to` — fake directory says member of `t1`,`t2`;
  scope `t1`; `may_allocate_for(t2)` false, `may_act_on(t2)` false.
- `a_superadmin_flag_never_widens_a_scoped_caller` (defensive; bench-tool never sets it).

Run: `cargo test -p kloudlite-workspaces scope`.
Commit: `Carry the parent login and a team scope on the api caller`

## Task 4 — `caller` accepts `bench-tool` with the four checks

Files: `crates/workspaces/src/api/mod.rs` (`caller` gains `method`/`path`: take `&Parts`-derived
`(Method, Uri)` via a new `caller_for(state, headers, method, path)`; `caller(state, headers)` keeps
its signature and REFUSES bench-tool, so a handler not migrated cannot be reached by the pod),
the handlers of the routes in `BENCH_TOOL_ROUTES` (switch to `caller_for`; extract with
`axum::http::Method` + `OriginalUri`), `crates/workspaces/tests/api_bench.rs`.

Arm, in order, each failure `401` and one `tracing::info!(owner, jti8, reason, "bench.tool.refused")`:
1. `expired`: `verify_bench_tool` fails with an expiry (other failures fall through to the normal
   user path and 401 as today).
2. `audience`: `!bench_tool_route(method, path)`.
3. `parent`: `!cli_token_live(state, &claims.parent)` (existing 30 s cache).
4. `bench`: GET `Bench/{claims.bench}`; refuse unless `spec.owner == sub`, `spec.team == team`,
   `desired_state != Stopped`, `access == BenchAccess::Full`. Written as one predicate
   `fn bench_admits_tool(b: &crd::Bench, sub, team) -> bool` so the pause plan only adds a variant.
Then `Caller { name: sub, superadmin: false, parent: None, scope: Some(team) }`.

Log line `http.write` for a bench-tool write carries `via=bench-tool` (find the writer with
`grep -rn "http.write" bins/api/src crates/workspaces/src`; add the field from a request extension
the arm inserts).

Tests (`tests/api_bench.rs`, reusing its fake kube routes and `bench_obj`):
- `a_bench_tool_token_lists_workspaces`.
- `a_bench_tool_token_is_refused_off_its_routes` — 401 on `GET /v1/cli/tokens` (directory router is
  another crate: assert via `bench_tool_route` false plus `/v1/bench/session`, `/v1/bench/tool-token`,
  `/v1/keys` 401 here).
- `a_bench_tool_token_dies_with_its_parent` — fake `is_live` false → 401.
- `a_bench_tool_token_dies_when_the_bench_stops` — `desiredState: Stopped` → 401.
- `a_bench_tool_token_is_refused_for_a_readonly_bench` — `access: ReadOnly` → 401.
- `a_bench_tool_token_gets_403_on_another_team`.
- unit `bench_admits_tool_checks_owner_team_state_access`.

Run: `cargo test -p kloudlite-workspaces --test api_bench`.
Commit: `Accept bench-tool tokens on the tool routes only`

## Task 5 — `POST/DELETE /v1/bench/tool-token` and the Secret

Files: `crates/workspaces/src/k8s/secrets.rs`, `crates/workspaces/src/api/bench.rs`,
`crates/workspaces/src/api/mod.rs` (route), `crates/workspaces/tests/api_bench.rs`.

Interface:
```rust
// k8s/secrets.rs
pub const BENCH_TOOL_SECRET: &str = "bench-tool";
pub fn bench_tool_secret(ns: &str, token: &str, exp: u64) -> Secret; // key `token`, annotation kloudlite.io/exp
// api/bench.rs
pub(crate) async fn mint_tool_token(State, HeaderMap, Query<TeamQuery>) -> Result<Response, Response>;   // 204
pub(crate) async fn revoke_tool_token(State, HeaderMap, Query<TeamQuery>) -> Result<Response, Response>; // 204, delete if present
```
`mint_tool_token`: `my_bench` → bench-tool caller is already 401 from `caller` (Task 4); a caller
with `parent == None` → 403 `sign in on the Kloudlite desktop app`; `Standing != Member` → 403;
`desired_state == Stopped` → 409 `bench is stopped; start it`. Mint with
`(caller.name, team, bench_id, parent)`, server-side apply into `crd::ws_namespace(owner, team)`
with field manager `kloudlite-api` (same call as `api/workspaces/keys.rs:142`). Patch failure →
503 with a fixed sentence; the kube error is logged by kind only, never through `kube_err`'s body.
Log `bench.tool_token.written { owner, team, jti8, parent8, exp }`.
`revoke_tool_token`: `my_bench`, delete the Secret, 404 counts as done.
`stop_bench`: after `set_desired`, best-effort delete of the Secret (warn on failure; check 4
already refuses).

Tests:
- `bench_tool_secret_carries_token_and_exp_only` (k8s unit, `crates/workspaces/src/k8s/tests/bench.rs`).
- `tool_token_writes_the_secret_in_the_bench_namespace` — asserts one SSA PATCH to
  `/api/v1/namespaces/{ns}/secrets/bench-tool`, 204, empty body.
- `tool_token_refuses_a_session_cookie` (403), `..._a_bench_tool_caller` (401),
  `..._a_stopped_bench` (409), `..._a_departed_member` (403).
- `a_failed_secret_write_does_not_echo_the_body` — fake patch 500 with the token in the error text;
  the response and captured logs do not contain it.
- `stopping_a_bench_deletes_its_tool_secret`.
- `deleting_the_tool_token_deletes_the_secret_and_tolerates_404`.

Run: `cargo test -p kloudlite-workspaces --test api_bench && cargo test -p kloudlite-workspaces k8s::`.
Commit: `Mint the bench tool token into a Secret in the bench namespace`

## Task 6 — ingress check for the api ship

Files: none in code; `deploy/kloudlite-web.yaml` read only.

Before rolling Tasks 1–5: `git show <pin>:deploy/kloudlite-web.yaml | grep -n 'environments|regions|quota|volumes|me'`
is non-empty, and after the roll `curl -s -o /dev/null -w '%{content_type} %{http_code}' https://{api-host}/v1/regions`
is `application/json… 401`. Record both outputs in the ship notes. If the line is missing, add
`bench` is NOT needed (the pod never calls `/v1/bench/*`); add only the missing tool families.
Commit: none unless the yaml changed (`Publish every bench tool route family on the api ingress`).

## Task 7 — mount the Secret in the bench pod

Files: `crates/workspaces/src/k8s/bench.rs`, `crates/workspaces/src/k8s/tests/bench.rs`,
`crates/workspaces/src/k8s/tests/attach.rs` (call-site arg), `bins/agent/src/lib.rs` (`Config`),
`bins/agent/src/controller/mod.rs` (`Ctx`), `bins/agent/src/controller/bench.rs` (call sites),
`deploy/k3s/agent-daemonset.yaml` and `deploy/kloudlite.yaml` agent env (`WS_API_URL`).

Interface: `bench_pod(b, id, pool, runtime_class, registry_host, api_url: &str, idle_secs)`;
`Config.api_url` from `WS_API_URL`, threaded like `registry_host`.
Volume `bench-tool`: Secret `BENCH_TOOL_SECRET`, `optional: true`, `default_mode: 0o444`, mounted
read-only at `/etc/kloudlite/bench-tool`. Env adds `KL_TOOL_TOKEN_FILE=/etc/kloudlite/bench-tool/token`
and `KL_API_URL=<api_url>`; omit `KL_API_URL` when empty (tools then fail closed).

Tests:
- `a_bench_pod_mounts_the_tool_secret_optional_and_read_only`.
- `a_bench_pod_carries_only_the_token_path_in_env` — no env value equals or contains a JWT shape
  (`eyJ`), `KL_TOOL_TOKEN_FILE` present.
- `a_workspace_pod_never_mounts_the_bench_tool_secret` (in `k8s/tests/pod.rs`).

Run: `cargo test -p kloudlite-workspaces k8s:: && cargo test -p kloudlite-agent controller::bench`.
Commit: `Project the bench tool token into the bench pod`

## Task 8 — desktop mints at Connect, renews every 5 minutes, deletes on sign-out

Files: `harness/src/connect/bench.ts`, `harness/src/main.ts`,
`harness/bench/test/desktop-bench.test.ts`, `harness/bench/test/desktop-controller.test.ts`.

Interface:
```ts
// connect/bench.ts
export async function mintToolToken(api: string, token: string, team: string, signal?: AbortSignal): Promise<void>; // POST, 204 ok; throws Expired on 401, Error(sentence) otherwise
export async function dropToolToken(api: string, token: string, team: string): Promise<void>; // DELETE, best effort, 5 s
```
`main.ts` `connect`: after `ensureBench`, `await mintToolToken(...)` (a failure is logged, not
fatal — the bench works, tools say sign in); start `toolTimer = setInterval(renew, 5 * 60_000)`;
the returned `disconnect` clears it. `revoke`: `await dropToolToken(c.api, c.token, team)` before
the existing `DELETE /v1/cli/tokens/{jti}`. A 401 on renew calls `auth.expired()`. The token never
crosses IPC (nothing returned to the renderer).

Tests (node, fake fetch as the existing desktop tests do):
- `connect mints a tool token after the bench is ensured`.
- `the tool token is renewed every five minutes and stops on disconnect` (fake timers).
- `sign-out deletes the tool token before revoking the login`.
- `a renew answered 401 expires the login`.

Run: `cd harness && node --test bench/test/desktop-bench.test.ts bench/test/desktop-controller.test.ts`.
Commit: `Mint and renew the bench tool token from the desktop`

## Task 9 — `kloudlite.ts` reads the token file; `/kl-login` deleted

Files: `harness/pi/kloudlite.ts`, `harness/bench/test/workspace-tools.test.ts`.

`call`: on every call read `process.env.KL_TOOL_TOKEN_FILE` and `process.env.KL_API_URL`; unset,
unreadable or empty → throw `Error("sign in on the Kloudlite desktop app")` with no fetch. Keep
`redirect: "error"` (add it; the current call lacks it) and the HTML guard. 401 → data
`sign in on the Kloudlite desktop app (your desktop session ended or the bench was stopped)`;
403 passes the server text. `kl_whoami` decodes the payload (no verify) → `{ username: sub, team, expires_at }`.
Delete `Config`, `dir`, `file`, `DEFAULT_API`, `load`, `save`, the `kl-login` command, the doc
comment's 30-day paragraph, and the `spawn`/`os` imports. `git grep kl-login` is empty after.

Tests (`workspace-tools.test.ts`, replace the `KL_CONFIG_DIR` fixture with a temp token file):
- `call re-reads the token file between calls` — rewrite the file between two calls, the fake
  server sees both tokens.
- `a missing token file throws the sign-in sentence without a request`.
- `a 401 maps to the sign-in sentence`.
- `whoami never returns the token`.

Run: `cd harness && node --test bench/test/workspace-tools.test.ts`.
Commit: `Authenticate bench tools with the projected token and delete kl-login`

## Task 10 — docs sweep

Files: any hit of `git grep -n "kl-login\|KL_CONFIG_DIR"` (harness README, `CLAUDE.md` if any),
and a CLAUDE.md paragraph under "Workspaces and environments" stating: the bench pod's `/v1`
credential is a 15-minute `bench-tool` JWT in Secret `bench-tool`, minted only by
`POST /v1/bench/tool-token` from a CLI login, accepted only on `BENCH_TOOL_ROUTES`, and dies with
its parent `jti`, a stop, or its expiry.
Check: `git grep -n kl-login` empty.
Commit: `Document the bench tool credential`

## Task 11 — one-time sweep of `(bench)` logins

Part A, tokens (directory binary). Files: `crates/pulls/src/directory/credentials.rs`,
`crates/api/src/credentials.rs`, `crates/api/src/lib.rs` (admin route next to `/api/admin/superadmins`).
```rust
// directory
pub async fn cli_tokens_with_device_suffix(&self, suffix: &str) -> Result<Vec<Credential>>; // Memory + Mongo
// api, superadmin only
POST /api/admin/bench-logins/revoke?dry_run=true|false -> { found: n, revoked: n, revoked_sha256: [hex] }
```
Revokes each through the same store call `revoke_cli_token` uses; per token one audit row
`bench.login.revoked {owner, jti8}` through the audit writer the superadmin routes already use
(find it from `add_superadmin`'s handler; if that route records none, log the line and record via
the admin process instead — decide in review, do not add a second audit mechanism).
Tests: Memory directory with three tokens (`x (bench)`, `x (desktop)`, `y (bench)`): dry run finds
2 and revokes 0; live run revokes 2; second run finds 0; a non-superadmin gets 403.
Run: `cargo test -p kloudlite-pulls directory && cargo test -p kloudlite-api credentials`.
Commit: `Add a superadmin sweep that revokes bench-made CLI logins`

Part B, files (agent). Files: `bins/agent/src/janitor.rs`, `deploy/k3s/agent-daemonset.yaml`,
`deploy/kloudlite.yaml` (optional ConfigMap mount).

Spec correction: `/kl-login` and `kl-connect login` write the SAME shape
(`bins/kl-connect/src/config.rs` `Config {api, token, expires_at, username}`), with no `device`
field, so a file cannot be judged by a label. It is judged by its token instead: Part A's response
also returns `revoked_sha256: [sha256(jti)]` (a digest, never the jti). The operator puts that list
in ConfigMap `bench-login-sweep` (key `jtis`, one hex digest per line), mounted optional into the
agent at `/etc/kloudlite/bench-login-sweep/jtis`.
```rust
fn sweep_bench_login_files(homes: &Path, revoked: &HashSet<String>) -> usize;
// for {homes}/{owner}/.config/kl-connect/config.json: decode the token payload without verifying,
// delete iff sha256(jti) is in `revoked`. Unreadable file, bad JSON, no jti → keep.
```
Runs on the janitor beat only when the list file exists and is non-empty (no flag needed; deleting
the ConfigMap ends it). Logs `bench.login.file.deleted {owner}`.
Tests (tempdir): a config whose jti digest is listed is deleted; an unlisted (laptop) one is kept;
bad JSON kept; empty list deletes nothing.
Run: `cargo test -p kloudlite-agent janitor`.
Commit: `Delete bench-made kl-connect configs from homes once`

Rollout for Task 11: run Part A dry run, read the count, run live, re-run (expect 0), sample one
revoked token on `GET /v1/workspaces` (expect 401). Create the ConfigMap, wait one janitor beat, then
on one node per region `find {pool}/homes -path '*/.config/kl-connect/config.json'` and check no hit's
jti digest is in the list. Delete the ConfigMap.

## Task 12 — SLO ids

Files: `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md` (held equal by its test),
`bins/slo/src/stages/bench.rs`, `bins/slo/src/stages/experience.rs` (dispatch), `bins/slo/src/suite.rs`
(group of the new ids: group 3 with the bench journey).

Catalogue rows (Suite::Hourly, stage "14 · Experience", feature "Benches"):
- `bench.tool.token` — "The probe's login mints a tool token and a `/v1/regions` call inside the bench pod answers JSON", `bound(120_000)`.
- `bench.tool.audience` — "The pod's token is refused on `/v1/bench/session` and `/v1/keys`", `avail(99.9)`.
- `bench.tool.revoked` — "After a stop the next pod call is 401 at once; after the parent login is revoked a pod call is 401 within 60 s", `bound(90_000)`.

Probe: `POST /v1/bench/tool-token` with the probe's CLI token; wait until the Secret file is
non-empty by exec in the bench pod (`cat /etc/kloudlite/bench-tool/token | wc -c`, never printing it,
up to 120 s); exec `node -e` fetch of `$KL_API_URL/v1/regions` with the file; judge status + JSON.
Audience: same exec against the two refused routes. Revoked: mint a throwaway CLI login for the
probe user, mint a tool token under it, revoke the login, poll the pod call for 401 ≤ 60 s; then
stop the bench and expect 401 on the first call (run last in the group, then restart for teardown).
Tests: catalogue/slo.md equality test passes; `group_of` for the three ids is 3.
Run: `cargo test -p kloudlite-workspaces slo && cargo test -p kloudlite-slo-bin`.
Commit: `Probe the bench tool token, its audience and its revocation`

## Task 13 — un-skip `bench.workspace.tool_roundtrip`

Files: `bins/slo/src/stages/bench.rs` (`tool_only`, `tool_roundtrip`).
The journey mints the tool token before the prompt (reuse Task 12's helper) so the workspace
session's `exec echo` reaches `/v1/workspaces/{id}/tools`. Remove any remaining skip path for it
other than the stub image; a missing token file is a FAIL, not a skip.
Test: existing suite unit tests pass; on the fleet, one hourly run shows the id passed with the
thread under `/bench/workspaces/{ws}/` (record the run id in the ship notes — "changed, unverified"
until then).
Run: `cargo test -p kloudlite-slo-bin`.
Commit: `Run the bench workspace tool round trip on the tool token`
