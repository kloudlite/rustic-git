# Desktop shells: a terminal into the bench and into each workspace

Owner ask (2026-09-16): "we should be able to access shells of these workspaces and bench" from the
desktop app. Owner approved approach 1 below ("bench as the shell hub") in chat.

## What exists

- The desktop terminal panel (`harness/src/renderer/components/terminal/`) is xterm.js with a local
  echo and no transport. Tabs are opened against a scope: the machine, or one workspace.
- The desktop holds one authenticated tunnel to the bench (`connect/tunnel.ts`, gateway →
  `harness-bench` on `BENCH_PORT` 7789). Every bench call and WebSocket goes through it, from the
  main process, with the `x-kl-tunnel` nonce and the loopback `Host` check.
- The bench reaches every workspace's tool server (`kl ide serve`, `IDE_PORT` 7788) inside the
  owner's namespace through the `allow-bench-tools` NetworkPolicy, and learns the address from
  `GET /v1/workspaces/{id}/tools?team=` with its projected tool token (`harness/pi/kloudlite.ts`).
- The tool server has `exec` (a job or a detached process with a byte ring) but no PTY, so no
  interactive shell, no job control, no TUI.
- The bench pod runs no sshd and has no volume; a bench is the developer's personal machine in the
  team.

## Design

One new wire surface, spoken twice, spliced once.

### The PTY protocol (shared by tool server and bench)

A WebSocket. Binary frames are raw bytes both ways: client → PTY stdin, PTY stdout → client. Text
frames are control JSON:

- client → server `{"resize":{"cols":N,"rows":N}}`
- server → client `{"exit":code}` once, then the server closes; `{"error":"…"}` then close when
  the shell could not start.

The first frame the client sends is a resize, so the shell never starts at 80×24 and reflows.
Nothing else: no scrollback replay, no session id, no reconnect to a running shell. A closed socket
kills the shell (SIGHUP to the process group). One socket, one shell, one life.

### The tool server: `GET /stream/pty` (crates/ide)

`kl ide serve` gains one route. It forks `$SHELL` (fallback `/bin/bash`, then `/bin/sh`) as a login
shell under a PTY, cwd = the workspace root, env = the process's own (the pod prelude already
built `PATH`, `CARGO_TARGET_DIR` and the rest for exactly this user), plus `TERM=xterm-256color`
and `COLORTERM=truecolor`. The PTY is opened with `libc::openpty` and the child with
`fork`/`setsid`/`ioctl(TIOCSCTTY)`/`execvp` — `libc` is already a dependency and `kl` is a musl
binary with a size budget, so no `portable-pty`. The master fd is driven from tokio with
`AsyncFd`. Read side: 64 KiB reads, forwarded as one binary frame each. Write side: bytes from
binary frames, in order. `resize` → `ioctl(TIOCSWINSZ)`. Child exit is observed by `waitpid` on
EIO/EOF of the master; the exit code becomes the `exit` frame.

Confinement is the namespace, exactly as `exec` today: the tool server is reachable only from the
person's bench and their own tunnel (`allow-bench-tools`; `ws_tools` hands the address to the owner
alone). A PTY is not a new capability over `exec` — `exec` already runs arbitrary commands as uid
1000 — it is the same capability with a controlling terminal. `/healthz` does not list it and
`GET /tools` never lists it (it is not a tool).

Limits: at most 8 live PTYs per tool server (a 9th is `{"error":"too many shells"}` + close);
the pod's own cgroup is the memory ceiling as it is for `exec`.

### The bench: `GET /pty?scope=bench|<workspace-id>` (harness/bench)

`harness-bench`'s upgrade handler admits a third path, `pty`, beside `events` and
`sessions/{id}/rpc`. It is reached only through the gateway tunnel, so it inherits the bench
session's authentication; there is no second credential.

- `scope=bench`: `node-pty` spawns `$SHELL` (fallback `bash`) as a login shell, cwd `$HOME`, env
  the bench's own minus `KL_TOOL_TOKEN_FILE` (a shell in the bench must not be one `cat` away from
  the platform token; the pi extensions read it, a person does not need to). `resize` →
  `pty.resize`. Exit → `{"exit":code}`.
- `scope=<workspace-id>`: the bench resolves the workspace's tool server exactly as
  `harness/pi/workspace-tools.ts` does (`resolveFromApi`, reused, not copied), opens
  `ws://{address}/stream/pty`, forwards the client's first resize and then splices frames both
  ways unchanged. Either side closing closes the other. A resolve failure (`409` between pods,
  stopped workspace) is `{"error":<the api's sentence>}` and close — the tab prints it and offers
  "Start it" nowhere; the workspace page already has start.

A live PTY holds the bench up (`idle.opened()` / `idle.closed()` like every other socket), because
a person with a shell open is using the machine. Bench sleep still kills every shell; that is the
documented ceiling (below).

Workspace id validation: the `segment()` rule already applied to route parameters — a workspace id
is `ws-[0-9a-f]{16}` — and anything else is `400` before any network call.

### The desktop (harness/src)

The renderer never opens sockets; the main process owns the tunnel. New IPC, one shell per id:

- `pty:open {id, scope, cols, rows}` → main opens `GET /pty?scope=` through the tunnel with the
  same headers as every bench WebSocket, sends the resize first, and pumps frames to
  `webContents.send("pty:data", id, Uint8Array)` / `("pty:exit", id, code)` / `("pty:error", id,
  msg)`.
- `pty:write {id, data: Uint8Array}`, `pty:resize {id, cols, rows}`, `pty:close {id}`.

`TerminalView` drops the local echo: `term.onData` → `pty:write`; `term.onResize` → `pty:resize`
(FitAddon on container resize, debounced 50 ms); `pty:data` → `term.write`. On exit the tab shows
`\r\n[process exited with code N]` dimmed and the tab title gets a dot; closing the tab or pressing
Enter on an exited tab closes it. On a tunnel drop the tab shows `\r\n[disconnected — reopen the
shell]`; there is no reconnect to the old shell (one socket, one shell). The banner stays but says
what it is: `kloudlite shell · <bench|workspace name> · <team>`.

Scopes: `scopesOf` gains the bench as the first scope (label "bench", sub "your machine in the
team") in place of the fixture "machine" scope, then every workspace of the person in the team,
from the same list the machine panel renders. A stopped workspace is listed but disabled with sub
"stopped".

### Image

`deploy/bench/Dockerfile`'s `deps` stage installs `python3 make g++` for `node-pty`'s native build
only if its published prebuild does not cover `linux-x64` glibc (the implementer checks
`node_modules/node-pty/prebuilds` after `npm ci`; if present, no toolchain is added). The runtime
stage is unchanged — `node-pty`'s `spawn-helper` needs no extra packages on bookworm-slim. The
bench pod's shell is `bash` from the base image; `SHELL` is set to `/bin/bash` in the pod env
(`crates/workspaces/src/k8s/bench.rs`) so a login shell is what a person expects.

### SLO

Two hourly ids in the bench stage (`crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`,
group 3 with the other bench ids): `bench.shell.roundtrip` — open `/pty?scope=bench`, send
`printf kl-%s ok\n`, see `kl-ok` within 5 s, exit 0 on `exit`; `bench.shell.workspace` — the same
through `/pty?scope=<the run's workspace>`, asserting the prompt's cwd is the workspace (`pwd`
prints `/workspace/...`). Both skip with the bench ids' existing reasons when the bench is not up.

## Out of scope (ceilings, stated)

- Reattaching to a shell after the socket drops or the bench sleeps: one socket, one shell. tmux in
  the image is the upgrade path if persistence is wanted; nothing in this design blocks it.
- Shells into an environment's service pods.
- Sharing one PTY between two devices.
- Any shell into a workspace from outside the bench path (kl-connect keeps ssh for that).

## Security summary

No new credential, no new port on the network, no new gateway ticket. The bench's PTY route is
behind the bench session tunnel; the tool server's PTY route is behind the namespace policy that
already admits `exec`. The one thing a bench shell must not expose that pi's extensions can read is
the platform tool token, and the bench shell's env drops the path to it (the file's mode already
keeps it from other uids; the env drop is so `env` in a screenshot does not point at it).
