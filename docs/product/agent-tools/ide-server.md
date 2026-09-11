# Tool server

Every workspace runs a tool server, `kl ide serve`, on loopback port 7788 inside the pod. It exposes the tools an agent uses against a codebase, read, write, edit, glob, grep, exec, watch and graft, as a plain HTTP API: `GET /tools` lists them with their JSON schemas, `POST /tools/{name}` runs one with the request body as its arguments. A session running outside the workspace reaches it through the ssh tunnel you already hold, so ssh is the authentication and there is no second credential. The server speaks no MCP itself: the session layer above speaks it once for every workspace you hold and forwards each call here.

## Connect

```bash [kl-connect]
kl-connect ws ide api
# kl-connect: api's tool API is at http://localhost:7788/tools while this runs.
```

Then, in another terminal:

```bash
curl http://localhost:7788/tools                                   # every tool and its schema
curl -X POST http://localhost:7788/tools/exec -d '{"cmd":"ls"}'    # {"exit_code":0,"stdout":"…"}
curl -X POST http://localhost:7788/tools/read -d '{"path":"Cargo.toml","limit":20}'
```

A tool's own answer is the body. Anything that stopped it from running is an HTTP status with `{"error": "…"}`: 404 an unknown tool, 400 bad arguments, 403 a path outside your home, 500 the tool failed. `--port` picks another local port when 7788 is taken.

## Tools

Paths are relative to the workspace directory, or absolute under `/home/kl`. Anything outside is refused naming the path.

| Tool | Does |
|---|---|
| `read` | A text file with line numbers; `offset` and `limit` page it. A binary file answers size and mime. |
| `write` | Create or overwrite a file, atomically; parents are created. |
| `edit` | Exact string replacements, all or nothing; each `old` must occur once unless `replace_all`. |
| `glob` | Files matching a pattern, gitignore-aware, newest first. |
| `grep` | Regex search; `mode` content, files or count; `glob` narrows; `context` adds lines. |
| `exec` | Run a command as the workspace user. A job waits and answers `exit_code`, `stdout`, `stderr` (timeout 120 s, max 600 s; at the timeout it answers `timed_out: true` at once). `detach: true` answers an id and the command becomes a process. |
| `process_list`, `process_output`, `process_write`, `process_kill` | Detached processes: list them, read output since a byte offset, write stdin, stop (TERM, then KILL). |
| `watch`, `watch_poll`, `watch_stop` | File-system changes under paths (create, modify, remove; reads are not events), or a command's output lines filtered by a regex; `once` ends at the first event. |
| `graft_find_code`, `graft_find_all`, `graft_trace_calls`, `graft_file_api`, `graft_repo_map` | The code graph: where a symbol is, every occurrence, who calls what, a file's signatures, the repo's shape. Freshness is the server's, read it from `/healthz`. |
| `graft_build`, `graft_blast` | Rebuild the graph (as a process); the blast radius of a diff. |

Two streams carry live output without polling: `GET /stream/process/{id}` and `GET /stream/watch/{id}`, WebSocket, one JSON object per frame. `GET /healthz` answers `ok`, the root, and the graph's state.

## Limits

| | |
|---|---|
| Read, write | 10 MiB per call |
| grep | 2 000 matches |
| glob | 5 000 paths |
| Process output kept | last 4 MiB per stream |
| Processes, watches | 32 each |
| Exec timeout | 120 s default, 600 s maximum |

## The code graph

Graft is a prebuilt index of every symbol, its `file:line` span and its call, reference and import edges, across languages. The server builds it on first start, refreshes it after every `write`, `edit` and finished `exec`, and watches the tree for changes made any other way (ssh, git). Queries answer from a current graph without waiting on one. The graph lives in `graft/` inside the workspace directory and is ignored by git globally, together with `.cache/` and `.direnv/`.

A deep build (LLM summaries of every symbol) is `graft_build` with `deep: true` and needs a provider key in the workspace; without one it answers `no_provider`.

## What it is not

No terminal: `exec` has no PTY in this version. No debugger. No gateway route yet: the tunnel is the only way in.
