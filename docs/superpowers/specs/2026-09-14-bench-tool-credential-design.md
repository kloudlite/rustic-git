# Bench tool credential — design

Status: draft for owner review · 2026-09-14 · builds on `2026-09-14-desktop-login-design.md`

## Problem

The bench's `kl_*` tools (`harness/pi/kloudlite.ts`, also `call` in `harness/pi/workspace-tools.ts`)
call `/v1/workspaces`, `/v1/environments`, `/v1/regions`, `/v1/quota`, `/v1/volumes`,
`/v1/builders/me` and `/v1/workspaces/{id}/tools`. The pod holds no platform credential, so a person
runs `/kl-login` inside the bench: a second device-code login whose 30-day CLI token is saved to
`~/.config/kl-connect/config.json`. On a bench that path is the NFS home `/home/kl`
(`k8s/bench.rs` `home_volume`), the same one every workspace of that owner mounts. So a full,
unscoped, month-long login ends up readable by anything running in any of their workspaces.

The owner asked why a session the desktop already authorized needs a second login. It doesn't.
`POST /v1/bench/session` is already called with the desktop's CLI token (which carries a `jti`)
and already proves who is using which bench in which team.

## Requirements

1. The desktop's CLI token never leaves the laptop's main process.
2. The pod's credential is minted by the platform and scoped to the bench owner and the bench's team.
   It is accepted only on the tool routes above, never on `/v1/cli/*`, `/v1/keys*`, `/v1/bench/*`,
   `/v1/internal/*` or admin.
3. The credential has a short TTL. It is renewed only while a desktop login is live and the bench is
   running. It dies when the parent login is revoked (the parent `jti` is checked on every use
   through `cli_token_live`) and when the bench is stopped or the person signs out.
4. It never appears in logs, in pod-spec env, or in a file another user can read. Every tool call
   reads it fresh.
5. It fails closed. With no credential or an expired one, the tool answers
   `sign in on the Kloudlite desktop app` and never starts a device code.
6. `/kl-login` is deleted.

## Approach A — derived `bench-tool` JWT, projected into a Secret (recommended)

**Token.** Add a new kind to `crates/core/src/jwt.rs` next to `bench-session`:

```
BenchToolClaims { sub: handle, team, bench, parent: cli jti, jti, iat, exp, typ: "bench-tool" }
BENCH_TOOL_TTL_SECS = 900
mint_bench_tool(handle, team, bench, parent) / verify_bench_tool(token)
```

`verify` and `verify_any_user` already refuse any other `typ` through `verify_typed`, so the
directory tier (`crates/api`), the admin process and the registry reject this kind with no change.
Only the one new arm in `caller` accepts it.

**Mint and renew.** Add `POST /v1/bench/tool-token?team=` in `api/bench.rs`. It uses `my_bench`, so
the rules on membership, departed members and 404s are unchanged. It refuses:

- a caller with no `jti` (a web session cookie), with 403;
- a `bench-tool` caller, with 401, so a pod cannot extend its own token;
- a bench whose `desiredState` is `Stopped`, with 409.

To support this, `caller` returns the `jti` it already computes, as `Caller.parent`. The route then
server-side-applies the Secret `bench-tool` (key `token`, annotation `kloudlite.io/exp`) into
`ws_namespace(owner, team)` and answers 204 with no body. The desktop calls it:

- once in Connect, right after `ensureBench`;
- every 5 minutes while the app is signed in and connected to that bench.

The 5-minute beat and 15-minute TTL leave room for the kubelet's Secret-volume sync, which takes
about 60–90 s. Renewal therefore stops when the desktop quits, disconnects or signs out. The pod
already exits after `benchIdleSecs` once no WebSocket is open.

**Delivery.** `k8s::bench_pod` adds a Secret volume `bench-tool` (`optional: true`, mode 0444),
mounted read-only at `/etc/kloudlite/bench-tool`. Only the file path goes into env:
`KL_TOOL_TOKEN_FILE=/etc/kloudlite/bench-tool/token` and `KL_API_URL`. This is the same projection
`user-key` uses: rotation means rewriting the Secret, and the kubelet swaps the file atomically with
a symlink. Mode 0444 has the same justification as `user_key_volume`: the kubelet owns the file as
root and the process runs as `kl`, inside a pod that belongs to one person.

The namespace belongs to one (owner, team) pair (`crd::ws_namespace`), so no other user's pod can
mount the Secret. Workspace pods in that namespace do not mount it either: the name is separate from
`user-key` on purpose. It never goes on the shared home. `kloudlite.ts` reads the file on every call
(a few hundred bytes), so rotation is picked up with no watcher.

**Use.** `caller` gets one extra arm for `typ == "bench-tool"`. Each check below runs on every
request:

1. **Audience.** Method and path must match `BENCH_TOOL_ROUTES`, a single table in `api/mod.rs`.
   It covers the seven route families above, including their `start`, `stop`, `push`, `clone`,
   `attach`, `detach`, `intercepts` and `packages` subroutes. Anything else gets 401. A test holds
   the table against the router, the same way `every_browse_route_is_routable` does, so every new
   `/v1` route has to be classified.
2. **Parent.** `cli_token_live(parent)` must be true. This is the existing 30 s positive cache, so a
   revoked desktop login stops working within 30 s.
3. **Bench.** A GET on `Bench/{bench}` must show `spec.owner == sub`, `spec.team == team`,
   `desiredState != Stopped` and `access == Full`. If it doesn't, the answer is 401. This is what
   makes stop immediate, even if a copy of the token is still in memory. A `ReadOnly` (departed)
   bench gets no tools.
4. **Scope.** `Caller { name: sub, superadmin: false, scope: Some(team) }`. The new `scope` field is
   read in `api/scope.rs`: `may_act_on` and `may_allocate_for` refuse any owner other than `sub` and
   `team` with 403. A tool call that names a different team fails even if the person belongs to it.

**Revoke.**

- `stop_bench` deletes the Secret, and check 3 already refuses the token.
- On sign-out, the desktop sends `DELETE /v1/bench/tool-token?team=` (best effort; it deletes the
  Secret) and then `DELETE /v1/cli/tokens/{jti}` (existing). Check 2 refuses the token within 30 s.
- Removal from a team makes `my_bench` return `Departed`, `ensure_access` sets `ReadOnly`, and
  check 3 refuses the token.
- If no desktop is live, the token expires within 15 minutes.

## Approach B — the desktop answers tool calls over the session

When the pod needs `/v1`, it sends an `api` request over the BenchClient WebSocket that is already
open. The desktop main process checks it against the same route table and replays it with its own
CLI token. A variant has the gateway inject the credential instead. That variant is really A with
an extra hop, because the gateway holds no user credential and would have to mint one.

| | A: derived token in a Secret | B: desktop proxies |
|---|---|---|
| New server surface | one JWT kind, one route, a `caller` arm, one pod volume | none |
| Tools with no desktop connected (a turn left running, a second device, the web) | work until TTL | fail: no device holds a token |
| Audience enforced by | the api (server side) | the desktop (client side; a modified app bypasses it) |
| Revocation | ≤30 s on parent, immediate on stop | immediate |
| Latency | pod → api | pod → gateway → laptop → api → back |
| Multiple devices on one bench | n/a | must choose which device answers; racy |
| Credential in the pod | yes, 15 min, narrowed | none |

**Recommendation: A.** The bench runs turns on the server side and is shared by every device, so a
credential that exists only while one particular laptop is connected contradicts that design. A
also enforces audience and scope on the server.

## Pod side (`harness/pi/kloudlite.ts`)

- Remove `load`, `save`, `dir`, `file`, `DEFAULT_API` and the `Config` type. Also remove the imports
  of `spawn` and `os` if nothing else uses them.
- `call` reads `KL_TOOL_TOKEN_FILE` and `KL_API_URL` on every call:
  - missing file, empty file or unset env → throw `sign in on the Kloudlite desktop app`;
  - 401 → the same message plus `(your desktop session ended or the bench was stopped)`;
  - 403 → pass the server's own sentence through.
- Keep the `redirect: "error"` and HTML-page guards.
- `kl_whoami` decodes the claims without verifying them and returns `{username, team, expires_at}`.
  It never returns the token.

## Removed code paths

- `harness/pi/kloudlite.ts`: the `pi.registerCommand("kl-login", …)` block (lines 163–195), the
  file-config helpers (22–35), the `/kl-login` strings on lines 41 and 160, and the doc comment
  (10–17) that describes the 30-day token.
- `harness/bench/test/workspace-tools.test.ts`: the `KL_CONFIG_DIR` fixture (129–142) becomes a
  `KL_TOOL_TOKEN_FILE` temp file.
- Any mention of `/kl-login` in the harness UI or catalogue. `git grep kl-login` is empty after the
  change.
- `POST /v1/cli/code` stays, because the desktop and `kl-connect` still use it. Only the bench's use
  of it goes away.

## Errors

| Case | Answer |
|---|---|
| No Secret yet (before first Connect, or after stop) | tool: sign-in message; no request sent |
| Token expired (desktop gone >15 min) | api 401 → tool: sign-in message |
| Parent revoked | api 401 within 30 s |
| Directory unreachable (`is_live` false) | 401; fails closed, same as CLI tokens today |
| Bench stopped, deleted or ReadOnly | 401 |
| Route outside `BENCH_TOOL_ROUTES` (`/v1/cli/tokens`, `/v1/bench/session`, `/v1/keys`) | 401 |
| Owner or team outside scope | 403 `bench tools act only for {handle} and {team}` |
| `tool-token` called with a session cookie or bench-tool token | 403 / 401 |
| Secret write fails | 503 to the desktop; the beat retries; the old token keeps working until exp |

## Audit and logging

- `bench.tool_token.written`: `owner`, `team`, `jti8`, `parent8` (first 8 hex characters), `exp`.
- `bench.tool.refused`: `owner`, `jti8`, and `reason ∈ {audience, parent, bench, scope, expired}`.
- The token, the Secret body and full `jti`s are never logged. `kube_err` on the Secret patch must not
  echo the request body; the `kube::Error` display does not include it, and a test checks that.
- Every `/v1` write the tool makes is already logged as `http.write` with the caller handle.
  `via=bench-tool` is added so a person can tell a model's action from their own.

## Tests

**Unit.**

- `jwt.rs`: mint and verify round trip; `verify` and `verify_any_user` refuse `bench-tool`;
  `verify_bench_tool` refuses `bench-session` and `cli`.
- `api`: the route-table test covers every `/v1` route. `tests/api_bench.rs` covers:
  - the pod token works on `GET /v1/workspaces`;
  - it gets 401 on `/v1/cli/tokens`, `/v1/bench/session`, `/v1/bench/tool-token` and `/v1/keys`;
  - 401 once `is_live` is false;
  - 401 once the bench is stopped;
  - 403 on another team;
  - `tool-token` with a session JWT gets 403.
- `k8s`: `bench_pod` mounts `bench-tool` as optional and read-only, with no token in env;
  `workspace_pod` does not mount it.
- `harness`: `node --test` checks that `call` re-reads the file between two calls (rotation), that a
  missing file gives the exact message and makes no fetch, and that 401 maps to the message.

**SLO** (catalogue.rs plus `deploy/slo.md`, hourly):

- `bench.tool.token`: the probe's CLI login calls tool-token, then execs `kl_regions`'s fetch inside
  the bench pod, and it passes.
- `bench.tool.audience`: the pod token on `/v1/cli/tokens` and `/v1/bench/session` is refused.
- `bench.tool.revoked`: the probe revokes the parent `jti`; within 60 s a pod call returns 401.
  After a stop the next call returns 401 at once.

## Rollout order

1. **api.** Add the JWT kind, the `caller` arm with the route table and scope, `tool-token`, the
   Secret delete in `stop_bench`, and the ingress. `deploy/kloudlite-web.yaml` line 252 does not
   list `environments|regions|quota|volumes` today, which is why `kloudlite.ts` has a guard for HTML
   pages; add them. Nothing mints a token yet, so this step is inert.
2. **agent/k8s.** Add the optional volume to `bench_pod`. A running pod is not replaced; the next
   wake picks it up.
3. **desktop.** Call tool-token at Connect and on the 5-minute beat; add the delete on sign-out.
4. **bench image.** New `kloudlite.ts` and `/kl-login` removed. Before this step the old tools still
   work with a stored login.
5. **Cleanup.** Revoke existing CLI tokens whose device label ends in `(bench)`, and delete
   `~/.config/kl-connect/config.json` from bench homes (see Q2).

## Open questions for the owner

1. **TTL and beat.** 15 min TTL with a 5 min renew. Is losing tools about 15 minutes after the
   laptop disconnects acceptable, or should the bench's own idle clock be the only limit? A longer
   limit means a longer TTL.
2. **Old bench logins.** Should the platform revoke the `(bench)` CLI tokens and delete the
   `config.json` written to NFS homes, or only tell people? The tokens are live for 30 days and
   readable from their workspaces.
3. **Scope.** Is the token limited to the bench's team plus the person's own handle (as proposed), or
   to the team only?
4. **ReadOnly benches.** Should a departed member's read-only bench get read-only tools (the GET
   routes) instead of none?
5. **API base.** Should the pod call `/v1` at the public host (which needs the ingress change) or at
   an in-region internal address? A bench in a k3s region has no in-cluster `kloudlite-api`.
