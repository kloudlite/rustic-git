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

Paths are relative to the workspace directory, or absolute under `/home/kl`. Anything outside is refused naming the path. Symlinks are followed before the check and the RESOLVED path is what the tool opens, so an answer names where the bytes actually are. A request body may be up to 11 MiB, which is the file tools' own 10 MiB limit plus room for the rest of the JSON; `write`, `edit` and `patch` write through a temp file beside the target whose name carries the process id, so two writers never share one.

| Tool | Does |
|---|---|
| `read` | A text file with line numbers; `offset` and `limit` page it. `paths` reads up to 50 files in one call, each answered on its own. A binary file answers size and mime. |
| `write` | Create or overwrite a file, atomically; parents are created. |
| `edit` | Exact string replacements, all or nothing across the call; `files: [{path, edits}]` edits up to 50 files atomically. Each `old` must occur once unless `replace_all`. |
| `patch` | Apply a unified diff; checked first so nothing is half-applied. Fewer tokens than `edit` for a large rewrite. |
| `glob` | Files matching a pattern, gitignore-aware, newest first. |
| `grep` | Regex search; `mode` content, files or count; `glob` narrows; `context` adds lines. |
| `exec` | Run a command as the workspace user. A job waits and answers `exit_code`, `stdout`, `stderr` (timeout 120 s, max 600 s; at the timeout it answers `timed_out: true` at once). `head` or `tail` keep only N lines of each stream; `quiet` answers the exit code alone, with stderr's last 20 lines on failure. `detach: true` answers an id and the command becomes a process. |
| `process_list`, `process_output`, `process_write`, `process_kill` | Detached processes: list them, read output since a byte offset, write stdin, stop (TERM, then KILL). |
| `watch`, `watch_poll`, `watch_stop` | File-system changes under paths (create, modify, remove; reads are not events), or a command's output lines filtered by a regex; `once` ends at the first event. |
| `graft_find_code`, `graft_find_all`, `graft_trace_calls`, `graft_file_api`, `graft_repo_map` | The code graph: where a symbol is, every occurrence, who calls what, a file's signatures, the repo's shape. Freshness is the server's, read it from `/healthz`. |
| `graft_build`, `graft_blast` | Rebuild the graph (as a process); the blast radius of a diff. |

Two streams carry live output without polling: `GET /stream/process/{id}` and `GET /stream/watch/{id}`, WebSocket, one JSON object per frame. `GET /healthz` answers `ok`, the root, and the graph's state.

## Workspace state

Separate from the tools: six read-only `GET` routes a console or IDE renders a workspace from. They are not in `/tools` and an agent does not need them. Git is read in-process (gitoxide), never by running `git`, so a tree with thousands of entries costs one index walk. Every answer carries an `ETag`, and a request with `If-None-Match` answers `304` with no body, so re-fetching after a change notification costs nothing when nothing changed. Errors are the same statuses as the tools: 403 outside your home, 404 nothing there, 400 a bad parameter, 413 a file over 10 MiB.

| Route | Answers |
|---|---|
| `GET /fs/tree?path=.&depth=1` | One directory level (`depth` up to 3, 5 000 entries): `name`, `kind` (file, dir, symlink), `size`, `mtime`, `ignored`, `git` (one letter: `M`, `A`, `D`, `R`, `?`, or empty), `target` for a symlink, nested `entries` past depth 1. Directories first. Hidden entries included; ignored directories (`.cache`, `graft`, `node_modules`) are shown but not descended into. A directory shows `M` when anything below it changed, `?` when everything below it is new. |
| `GET /fs/stat?path=` | One row of the tree, plus `mime` for a file. |
| `GET /fs/file?path=&at=` | The bytes, with the sniffed `Content-Type`. `at=HEAD` (any ref, or `index`) answers the committed copy: the other side of a diff. Binary files are served as they are. |
| `GET /fs/git` | `repo`, `branch` (null when detached), `head`, `upstream`, `ahead`, `behind`, `dirty`, `stashes`. A directory that is not a repository answers `repo: false`. |
| `GET /fs/changes` | Every changed path with git's two status letters (`index`, `worktree`), `renamed_from`, and `additions`/`deletions`/`binary` — status and counts in one call. |
| `GET /fs/diff?path=&against=HEAD` | One file's unified diff (`against` HEAD, `index` or `staged`); a new file diffs against `/dev/null`. No `path` is the whole tree. Capped at 200 000 bytes, `truncated: true` past it. |

For live updates, `watch` on `.` with `GET /stream/watch/{id}` names each changed path; re-fetch that node with `/fs/stat`, its parent with `/fs/tree`, and `/fs/changes`.

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
