# Claude Code bench — design

Date: 2026-10-05. Supersedes `2026-09-22-sys1-sessions-design.md`.

## Why

We stop building our own agent harness and runtime (the sys-1 engine and operations executor in
`harness/bench`). The bench runs Claude Code instead. Its mods give us message middleware and
rendering, and many people already know the tool. The UX reference is the owner's demo mod at
`~/.claude/dev-mods/21c1f1e6-74d4-4bae-94cc-da1b6274cdfe/kloudlite/` (pane, workspace list,
transcripts, subagent tree, environment and intercepts, tasks). This spec makes that demo real.

## Rulings this design rests on

- **Provider from outside.** Claude Code gets `ANTHROPIC_BASE_URL` plus a token. DeepSeek and others
  sit behind that URL. We write no provider code and no key API.
- **Token per user.** Platform-wide base URL; a per-user JWT minted from `kloudlite-jwt`,
  re-minted every `KEYS_RESYNC_SECS` like `registry-token`.
- **All sessions run in the bench pod.** Workspace pods only expose tools (`kl ide serve`, :7788).
- **One session per workspace**, and each subagent of a workspace session is shown as its own
  session in the pane.
- **One bench per user.** Team workspaces never share a bench; each teammate has their own.
  Identity stays `crd::bench_id(owner, team)`: personal, with the team as quota and region scope.
- **Access:** ttyd in the browser (through the gateway, embedded in the web console) and
  `kl-connect bench` over SSH, both attached to the same tmux session running `claude`. This
  reverses Task S's "bench pod gets no shell": the only terminal is that tmux session.
- **Tools wired as Claude Code tools.** Workspace tools are registered as Claude Code's own tools,
  not hidden behind the built-ins.
- **sys-1 is deleted**, with the operations executor, contracts and judgments.

## 1. Tool wiring

The kloudlite mod, on `session.start`:

1. Reads `KL_WORKSPACE` (set by `kl-sessions` per session). If it is unset, this is the main session:
   it has no workspace tools, and `tool.call {tool:'Agent'}` without `e.agentId` is denied, as in
   the demo ("send the task to a workspace session").
2. Gets the tool server address from `GET /v1/workspaces/{id}/tools`.
3. Gets `GET {addr}/tools` and calls `$.tool.register` once per tool. Schemas come from the tool
   server, so the mod holds no copy. A tool added to `crates/ide` (graft's included) appears with
   no mod change.

Each registered tool's handler sends `POST {addr}/tools/{name}` with the input unchanged and
`Authorization: Bearer <bench tool token>`. It returns the result unchanged, or the `{"error"}` body
as a tool error. `process_*` and `watch*` use the same request and response. The stream WebSockets
are for the pane only. The timeout matches the job, up to the mods 10-minute ceiling. Longer
work is `exec` with `detach: true` followed by `process_*`.

**Built-ins off.** Read, Write, Edit, MultiEdit, Bash, Grep, Glob, NotebookEdit and WebFetch would
act on the bench, not the workspace. `kl-sessions` passes them as `disallowedTools`, and
`tool.check` denies them as a backstop. Kept: Agent, TodoWrite, WebSearch. Subagents inherit the
registered tools. `tool.call` with `e.agentId` feeds the pane.

**Fails closed.** An unreachable tool server returns an error naming the workspace and its state
("workspace ws-… is stopped; start it"). Nothing falls back to the bench filesystem.
Registration runs under `.catch`, so if the mod fails the session has no tools rather than the
built-ins.

**Context.** `prompt.context` adds the workspace's `~/workspace/CLAUDE.md` (via `read`) and the
`/fs/git` status, once per turn.

**Network.** Bench pod to workspace pod IP :7788, allowed by the existing `allow-bench-tools`
NetworkPolicy. The address is given to the owner only. The bench runs as that owner.

**Auth.** Unchanged. The bench already gets a tool token (`api::bench::mint_tool_token`, revoked on
stop). `crates/ide/src/auth.rs` stays as is.

**Spike, first task of the plan.** Check how a registered tool's call and result render in the
transcript, given that the built-in diff view is lost for `edit` and `patch`. If it is unreadable,
the mod draws the `ToolUse`/`ToolResult` sites for `edit`, `patch` and `exec`.

## 2. The bench pod

**Image** (Dagger-built, like today): Node 22, a pinned `claude` CLI and a pinned
`@anthropic-ai/claude-agent-sdk`. The mod is baked in at `/opt/kloudlite/mod` with
`CLAUDE_CODE_PLUGIN_DIRS=/opt/kloudlite/mod`. No other agent code.

**runit services** (supervisor from Task R):

| Service | Command |
|---|---|
| `claude` | `tmux new -A -s kl claude`: the main session, with the pane |
| `ttyd` | `ttyd -W tmux attach -t kl`, reached through the gateway |
| `sshd` | `ForceCommand tmux new -A -s kl claude`, for `kl-connect bench`; no other shell |
| `kl-sessions` | the demo's `service/server.mjs`, hardened, on `127.0.0.1:8917` |

**`kl-sessions`**: one warm Agent SDK `query()` per workspace, with streaming input and one message
in flight while the rest queue, as in the demo.

- Options per session: `env.KL_WORKSPACE={id}`, `cwd ~/sessions/{id}`, `plugins` loading
  `/opt/kloudlite/mod`, `disallowedTools` from section 1, and `permissionMode: 'bypassPermissions'`.
  Bypass is safe because the tool server confines every path under the workspace home.
- **Subagents.** Messages carrying `parent_tool_use_id` are grouped per subagent. `GET /state` returns
  `{ws: {lines, busy, queued, agents: [{id, label, status, lines}]}}`.
- **Other endpoints.** `POST /send {ws, text}` and `POST /send {ws, agentId, text}`.
  `GET /idle` returns `{clients, busy}`, where `clients` is the count from `tmux list-clients`.
- **Persistence.** `~/.claude` and `~/sessions/sessions.json` are on the bench Volume (`/home/kl`),
  so `resume` works across a wake.
- **Errors.** A session that dies is marked `errored` with `s:(error) <cause>` and is restarted
  (resumed) on its next message.

**Provider.**

- **Base URL.** New `ClusterSettings` field `benchProviderUrl` (`Mark::Boot`), stamped into the
  bench pod as `ANTHROPIC_BASE_URL` at create.
- **Token.** `api` mints `provider-token` into the `user-key` Secret (per-user JWT from
  `kloudlite-jwt`, audience `provider`) on the keys beat.
- **Reading it.** `apiKeyHelper` in the image's managed settings reads the mounted file on every
  call, so rotation needs no restart.
- **Scope.** The proxy behind the URL is not ours.

**Idle.** The bench is idle when `GET /idle` shows `clients == 0 && !busy`. The agent's existing
`bench_idle_secs` check reads this in place of the old bench server's signal.

## 3. Pane, deletions, probes

**Pane data.** The demo layout, with real data. The mod polls every 2 s with the bench's platform
token:

- `GET /v1/workspaces`: list, state, current activity.
- `GET /v1/environments/{id}`: services, status, ports, `status.intercepted_by`, shown as
  `⇄ svc:port`.
- `kl-sessions /state`: transcripts, queue, subagents.
- `/env` picks from `/v1/environments`, replacing the hardcoded list.
- TASKS is `process_list` on that workspace's tool server, and kill is `process_kill`.
- The mod keeps no state beyond view atoms.

**Deleted:**

- `harness/bench` (sys-1 engine, operations executor, contracts, judgments, bench server), and
  anything under `harness/` imported only by it.
- `api::bench::bench_session` with its routes and ingress paths.
- The Kompress setting (`crd/settings.rs`) and its pod env.
- The `kloudlite-bench-engine` Secret reference. The owner deletes the live Secret by hand.
- The Dagger bench image's harness build, replaced by the image in section 2.

No compat code and no migration (dev phase).

**SLO probes** (`crates/workspaces/src/slo/catalogue.rs` and `deploy/slo.md`, kept equal by the
existing test):

| Probe | Change |
|---|---|
| `bench.session.roundtrip`, `bench.two_clients` | retired |
| `bench.tunnel` | ttyd answers through the gateway, and `kl-connect bench` reaches tmux |
| `bench.idle.wake` | `/state` session list and a transcript read back unchanged after a wake |
| `bench.claude.tool_roundtrip` (new) | `POST /send` on a probe workspace asks to read `~/workspace/README.md`; the transcript shows a `read` call answered from the workspace (also proves provider URL and token) |
| `bench.builtin.refused` (new) | a prompt forcing built-in `Bash` is denied |
| `bench.tool.token`/`audience`/`revoked` | kept |

## Out of scope

- The provider proxy itself, and its model routing.
- Licensing for multi-tenant hosting of Claude Code. The owner checks this against Anthropic's
  Commercial Terms before go-live.
- Web console changes beyond embedding the ttyd URL.
