# `kl ide serve` — the workspace tool server (spec, draft 2)

Status: approved 2026-09-11; plan at `docs/superpowers/plans/2026-09-11-kl-ide-serve.md`.

## Why

An agent session running OUTSIDE a workspace (Claude Code on a laptop, a CI agent, the console)
needs the same handful of tools it uses locally — read, write, edit, glob, grep, exec, watch,
language intelligence — executed INSIDE the workspace, against the tree and the toolchain that
live there. Today the only way in is ssh, which means every tool is a shell command re-implemented
by the caller. This server makes the tools first-class: typed, streamed, bounded, and the same
for every client.

Reference shapes: Claude Code's built-in tools (the surface), Daytona's agent toolbox (the
precedent), MCP (the wire protocol every agent already speaks).

## Scope

In: the workspace-facing tools from `/tmp/claude-code-tools-and-config.md` §1a.
Out (later or never): PTY terminals for humans, DAP debugging, preview ports, notebooks (an
edit is an edit), git worktrees (an exec), LSP (graft instead, below), anything Claude-only (§1b, §1c).

## Placement

- Runs inside the workspace pod as user `kl`, started by the workspace image next to sshd
  (`kl ide serve`), listening on `127.0.0.1:7788`. Loopback only: the only way to reach it is an
  ssh tunnel the person already holds (`kl-connect ws ssh api -- -L 7788:localhost:7788`, or
  a `kl-connect ws ide api` that does exactly that), so ssh IS the authentication and no second
  credential exists. A custom-image workspace that does not ship `kl` simply has no server.
- Phase 2 (separate spec): the gateway routes `/ide/{ws}` to the pod with an `ide-session` ticket,
  the way `/tunnel/{ws}` routes ssh, so the console and cloud agents reach it without ssh.

## Protocol

MCP over streamable HTTP at `POST /mcp` — an agent adds it with `claude mcp add --transport http
workspace http://localhost:7788/mcp` and the tools appear as `mcp__workspace__*`. Tool results
are one-shot by MCP's nature, so the two streaming needs get a second, plain endpoint:

- `GET /stream/process/{id}` — WebSocket, frames of stdout/stderr/exit for one process.
- `GET /stream/watch/{id}` — WebSocket, events for one watch.

Both are also exposed as MCP tools that RETURN the accumulated state (`process_output`,
`watch_poll`), so a client without WebSocket still works, one poll at a time. `GET /healthz`
answers `{ "ok": true, "root": "/home/kl/workspaces/api", "graph": "ready" }`.

## Tools

Paths are absolute or relative to the tree root (`/home/kl/workspaces/{name}`). Every path is
canonicalised and must stay under `/home/kl`; anything else is `EACCES` with the path named.
Limits are per call: read ≤ 10 MiB, write ≤ 10 MiB, grep ≤ 2 000 matches, glob ≤ 5 000 paths,
process output ring buffer 4 MiB, at most 32 live processes and 32 watches.

| Tool | Input | Output | Notes |
|---|---|---|---|
| `read` | `path`, `offset?` (line), `limit?` (lines) | `content` (numbered lines), `total_lines`, `truncated` | text only; a binary file answers `mime` and `size`, no bytes |
| `write` | `path`, `content` | `bytes`, `created` (bool) | atomic: temp file + rename; parent dirs created |
| `edit` | `path`, `edits: [{old, new, replace_all?}]` | `applied` (count) | all-or-nothing; `old` must match exactly once unless `replace_all`; a miss names the edit index |
| `glob` | `pattern`, `cwd?` | `paths[]` sorted by mtime desc, `truncated` | gitignore-aware |
| `grep` | `pattern`, `cwd?`, `glob?`, `context?`, `mode` = files \| content \| count, `max?` | matches with `path`, `line`, `text` | ripgrep semantics (regex, case flags); ripgrep binary from the profile, fallback to a Rust regex walk |
| `exec` | `cmd` (argv or shell string), `cwd?`, `env?`, `timeout_ms?` (default 120 000, max 600 000), `detach?` | job: `exit_code`, `stdout`, `stderr` (each capped, `truncated` flag); process: `id` | `detach: true` returns at once; the process becomes a `process` below |
| `process_list` | — | `[{id, cmd, started_at, state: running\|exited, exit_code?, pty}]` | |
| `process_output` | `id`, `since?` (byte offset) | `stdout`, `stderr`, `next` (offset), `state`, `exit_code?` | poll form of the stream |
| `process_write` | `id`, `data` | `bytes` | stdin, for a pty session or an interactive tool |
| `process_kill` | `id`, `signal?` = TERM \| KILL | `state` | TERM, then KILL after 5 s |
| `watch` | `paths[]` or `cmd` + `pattern?` (regex), `once?` | `id` | file/dir change events, or a command's output filtered by a regex; `once` ends at the first match |
| `watch_poll` | `id`, `since?` | `events[]`, `next`, `state` | poll form of the stream |
| `watch_stop` | `id` | `state` | |

### Code intelligence: graft, not LSP

Graft (v0.18) is a prebuilt symbol graph — every definition with its `file:line` span, call /
reference / import / implements edges, per-file cards, an optional LLM concept map — refreshed
against uncommitted edits before each query. `kl ide serve` exposes the part of it an agent uses:
the six tools graft's own MCP server offers are proxied (a `graft mcp` child on stdio, spawned on
first use), and two CLI-only capabilities (`build`, `blast`) are wrapped as tools that run
`graft <cmd> --json` and return the JSON. One MCP endpoint, one namespace.

**Index freshness is the server's job.** There is no Claude Code in the workspace and so no hook
runner; the triggers graft's `init` would wire into an agent are wired into the server's own
tool calls instead:

- `write`, `edit` and every `exec` that exits run graft's incremental refresh afterwards — the
  same code path graft itself runs before a query (`@nanonets/graft/dist/graph/refresh.js`,
  invoked as `graft build` with reuse: unchanged files replay from the extraction cache, only
  the touched files are re-parsed). Small edits refresh in well under a second.
- A file watcher on the tree catches changes made any other way — an ssh session, a `git
  checkout`, a detached process still writing — and runs the same refresh, debounced to once
  per 2 s, ignoring `graft/`, `.git/`, `target/`, `node_modules/` and whatever `.gitignore`
  excludes.
- Queries still run graft's own freshness check first (its default), so a refresh the watcher
  has not finished yet is caught at the query; that check is a size+mtime scan and costs little
  when nothing moved.
- On server start: a plain `graft build` if `{tree}/graft` is missing, else `graft check`; a
  drifted graph is refreshed before the server reports healthy, and `GET /healthz` carries
  `graph: ready | building | drifted`.
- `graft/` is ignored by git GLOBALLY in the workspace, never per repository: the image ships
  `/home/kl/.config/git/ignore` (git's `core.excludesFile` default) with `graft/`, and the
  server refuses to start if that line is missing. A per-repo `.gitignore` entry would be a
  diff the person did not ask for on every repository they open; the global file is derived
  state's proper home. The graph is rebuilt on a restore or a clone from the bytes that came
  with it, and a `git status` never shows it.

The server owns one tree, so `dir` is never a parameter.

| Tool | graft command | Input | Output |
|---|---|---|---|
| `graft_find_code` | `ask` | `query`, `limit?` (8), `full?`, `in?` (scope prefix) | ranked nodes with `file:line`, source inlined |
| `graft_find_all` | `grep` | `pattern`, `in?`, `ignore_case?`, `fixed?` | every hit grouped by enclosing symbol, ranked by coupling |
| `graft_trace_calls` | `callers` | `symbol`, `direction?` in\|out, `depth?` N\|all, `in?` | callers, callees, or the transitive closure |
| `graft_file_api` | `skeleton` | `file` | every signature and span of one file |
| `graft_repo_map` | `map` | `max_dirs?` (16) | directory clusters, hubs, hotspots |
| `graft_check_freshness` | `check --json` | — | the drift report |
| `graft_build` | `build` | `deep?`, `no_reuse?` | a `process` id; on exit: files, nodes, edges, duration |
| `graft_blast` | `blast --format json` | `base?` (git ref, default HEAD), `depth?` N\|all | what depends on the lines the diff touched |

Server flags, not tools (fixed for the life of the workspace): `--graft-dir`, `--extensions`,
`--follow-submodules`, `--follow-nested-repos`, `GRAFT_REFRESH`.

Not exposed: `viz` (a human UI; run it through `exec` if wanted), `stats` (the CLI's own session
bookkeeping), `blast --name/--owners/--export-viz` (PR-comment dressing), `init`, `uninstall`,
`upgrade`, `version`, `telemetry` (`DO_NOT_TRACK=1`).

Runtime: `node` and the graft package in the workspace image (or the Nix profile). The graph
lives at `{tree}/graft/` as graft expects. A workspace whose tree has no graph gets one built by
the first `graft_*` call (plain build, seconds to a minute; the call answers `building` with the
process id, and later calls answer normally). `--deep` needs `GRAFT_PROVIDER` / `GRAFT_MODEL` /
`GRAFT_API_KEY`, supplied to the pod as an optional owner Secret projected like the registry
token; absent, `graft_build {deep:true}` answers `no_provider` and nothing else changes.

Diagnostics (type errors, compile errors) come from `exec` (`cargo check`, `tsc`); hover types
and rename are out of scope.

Naming: `job` = a bounded exec that returns its result; `process` = a detached exec with an id,
running until it exits or is killed. A `pty: true` on `exec` allocates a terminal for the
process; the stream then carries one merged byte channel.

## Semantics that matter

- Exec runs through the login environment the workspace already builds (`PATH` with the
  profile, `CARGO_TARGET_DIR`, …), as `kl`, in the tree root unless `cwd` says otherwise.
- Output is UTF-8 with lossy replacement; the ring buffer keeps the LAST 4 MiB, and `truncated`
  says bytes were dropped at the front.
- A detached process survives client disconnects and server restarts do NOT preserve it: on
  restart the list is empty and orphans are reaped by process group.
- Edits are byte-exact on the file as read; a file that changed since the client's last `read`
  simply fails the match, which is the right answer.
- Everything is single-tenant: one server, one user, one tree. No auth inside; the ssh tunnel is
  the boundary. The server refuses to start if it is not running as `kl` or if `HOME` is not
  `/home/kl`.

## Observability

Every call logs one `ide.call` line (tool, ms, ok, bytes) to stderr, which the pod log ships
already; a process start and exit log `ide.process`. `GET /metrics` is not needed: the pod log is
the record.

## Binary

`kl` is a musl binary with `clap` only. This adds `tokio`, `axum` (HTTP + WebSocket), `serde`,
`ignore`/`globset` (glob, gitignore), `regex`, `notify` (watch), `portable-pty` (pty), and an
MCP server layer (the `rmcp` crate, or ~300 lines of JSON-RPC by hand). Roughly +6–8 MiB. The
alternative is a second binary `kl-ide` in the image; same code, one more artefact to ship.

## Probe

Two hourly ids: `ide.serve.up` (tunnel, `GET /healthz` within 2 s) and `ide.exec` (an `exec` of
`true` and a `read` of a written file, within 5 s).

## Decisions

MCP over streamable HTTP plus two WebSocket streams; phase 1 reach is the ssh tunnel only; the server lives in `kl`; no PTY in the first cut (`pty: true` answers `unsupported`); graft proxied from `@nanonets/graft@0.18`. Taken as defaults on 2026-09-11 when the owner said "go on with next plan".
