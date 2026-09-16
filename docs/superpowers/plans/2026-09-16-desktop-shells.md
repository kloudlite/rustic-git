# Desktop shells implementation plan

> **For agentic workers:** implemented task by task by Opus implementers dispatched from the main
> session (Fable), which reviews each diff itself. Steps use `- [ ]` for tracking.

**Goal:** a person opens a real shell into their bench or into any of their workspaces from the
desktop app's terminal panel.

**Architecture:** one PTY WebSocket protocol (binary = bytes, text = `{resize}` / `{exit}` /
`{error}`), served by the tool server (`kl ide serve`, `GET /stream/pty`, libc openpty) and by the
bench (`harness-bench`, `GET /pty?scope=bench|<ws-id>`, node-pty for the bench, a splice to the
workspace's tool server otherwise). The desktop main process opens `/pty` through the existing
tunnel and pumps frames to xterm over IPC.

**Spec:** `docs/superpowers/specs/2026-09-16-desktop-shells-design.md` — the binding authority.

## Global constraints

- Worktree `/Volumes/kdisk/rustic-git-wt/desktop-login`, branch `desktop-login`. Prefix every
  shell command with `cd` there; the cwd resets between calls.
- Rust: `cargo clippy --workspace --all-targets -- -D warnings` and the touched crate's tests. Run
  cargo with `CARGO_TARGET_DIR` unset (it is configured for `/Volumes/kdisk`). Never `cargo` in
  `/Users/karthik/rustic-git`.
- Harness: `cd harness && npm run typecheck && npm run bench:test && npm run build`.
- Comments explain WHY; module docs carry context; no new file over ~800 lines; `// ponytail:`
  markers kept.
- Commits: imperative sentence case, NO attribution lines of any kind. Do not push.
- No new Rust dependency (libc is already in `crates/ide`). One new npm dependency: `node-pty`.
- Wire names are exactly the spec's: `resize`, `cols`, `rows`, `exit`, `error`, `scope`.
- Never log or print the bench tool token or its path.

---

### Task 1: the tool server's PTY route (Rust)

**Files**
- Create: `crates/ide/src/pty.rs`
- Modify: `crates/ide/src/server.rs` (route), `crates/ide/src/lib.rs` (`mod pty;`, module map in
  the `//!` header), `crates/ide/Cargo.toml` only if a `tokio` feature (`net`, `io-util`) is missing.

**Interfaces produced:** `GET /stream/pty` WebSocket, protocol per spec. `pty::Limits { max: 8 }`
held in `App` as an `AtomicUsize` of live shells.

- [ ] `pty.rs`: `pub struct Pty { master: tokio::io::unix::AsyncFd<OwnedFd>, child: libc::pid_t }`;
      `pub fn spawn(root: &Path, cols: u16, rows: u16) -> std::io::Result<Pty>` — `openpty`, fork;
      child: `setsid`, `ioctl(slave, TIOCSCTTY)`, dup2 slave onto 0/1/2, close others,
      `chdir(root)`, `setenv TERM=xterm-256color COLORTERM=truecolor`, `execvp` of `$SHELL` then
      `/bin/bash` then `/bin/sh`, argv0 `-<basename>` (login shell). Parent: close slave, set
      master `O_NONBLOCK`, `TIOCSWINSZ` to cols×rows. `pub fn resize(&self, cols, rows)`;
      `pub async fn read(&self, buf) -> io::Result<usize>` (EIO = child gone → `Ok(0)`);
      `pub async fn write_all(&self, bytes)`; `pub fn wait(&mut self) -> Option<i32>` (`waitpid`
      WNOHANG, then blocking after EOF); `Drop` sends `SIGHUP` to `-pid` (the process group) and
      reaps.
- [ ] `pub async fn handler(State(app), ws: WebSocketUpgrade) -> Response`: refuse with
      `{"error":"too many shells"}` when `app.shells >= 8` (send as first text frame, then close);
      otherwise `on_upgrade(pump)`. `pump`: wait ≤ 2 s for the first text frame; if it is a
      `resize` use it, else 80×24. Spawn; on spawn error send `{"error":"<io error>"}` and close.
      Then `select!` over: master read → binary frame; socket message: Binary → `write_all`; Text
      `resize` → `resize`; Close/None → break. On read EOF: `wait()` → send `{"exit":code}`, close.
      Decrement `app.shells` on every exit path.
- [ ] `server.rs`: `.route("/stream/pty", get(crate::pty::handler))`. `GET /tools` must not
      list it (it is not a tool; nothing to change if the list is built from the tool table — add
      an assertion to the existing route test that `/tools` output has no `pty`).
- [ ] Tests (inline `#[cfg(test)]`, `#[tokio::test]`): (a) spawn with `SHELL=/bin/sh`, write
      `printf kl-%s ok\n; exit 3\n`, read until EOF, assert output contains `kl-ok`, `wait()` is
      `Some(3)`; (b) `resize(120, 40)` then `stty size` prints `40 120`; (c) drop while the shell
      sleeps → child reaped within 1 s (`kill(pid, 0)` fails with ESRCH); (d) the 9th concurrent
      `/stream/pty` upgrade through `router()` gets the `too many shells` frame (use
      `tokio_tungstenite` from dev-deps if present in the workspace; else drive `pump` directly
      with an in-process axum server on 127.0.0.1:0).
- [ ] Gates: `cargo test -p kloudlite-ide`, workspace clippy. Commit: `Serve a PTY from the
      workspace tool server`.

### Task 2: the bench's PTY route and the workspace splice (TypeScript)

**Files**
- Create: `harness/bench/src/pty.ts`
- Modify: `harness/bench/src/server.ts` (upgrade path `pty`), `harness/bench/package.json` and
  `harness/package.json` (`node-pty` dependency — installed in `harness/`, the one lockfile),
  `harness/pi/workspace-tools.ts` only to `export` `resolveFromApi` if it is not exported (it is:
  keep), `harness/bench/test/pty.test.ts` (new), `harness/bench/test/server.test.ts` (one
  upgrade-refusal case).

**Interfaces produced:** `GET /pty?scope=bench|<ws-id>` on the bench, protocol per spec.
`pty.ts` exports `attachBenchShell(ws: WebSocket, env: NodeJS.ProcessEnv, first: Resize)` and
`spliceWorkspaceShell(ws: WebSocket, address: string, first: Resize)` and `readFirstResize(ws,
timeoutMs): Promise<Resize>` where `type Resize = { cols: number; rows: number }`.

- [ ] `server.ts` upgrade: accept `p.length === 1 && p[0] === "pty"`. Parse `scope` from the
      query: `bench` or `/^ws-[0-9a-f]{16}$/`; anything else → write `HTTP/1.1 400` on the raw
      socket and destroy (before `handleUpgrade`). After `handleUpgrade`: `idle.opened()`,
      `w.on("close", idle.closed)`, `w.on("error", () => {})`, then
      `readFirstResize(w, 2000)` (default `{cols:80, rows:24}` on timeout) and dispatch to
      `attachBenchShell(w, process.env, first)` or `resolveFromApi(ws).then(a =>
      spliceWorkspaceShell(w, a, first), e => (w.send(JSON.stringify({error: e.message})),
      w.close()))`. The resolve function is injectable through `serve()`'s options so tests pass
      a fake (`resolveTools?: (ws: string) => Promise<string>`).
- [ ] `attachBenchShell`: `import * as pty from "node-pty"`; env = `{...env}` with
      `KL_TOOL_TOKEN_FILE` deleted and `TERM=xterm-256color`, `COLORTERM=truecolor`; shell =
      `env.SHELL ?? "/bin/bash"`, args `["-l"]`, `cwd: env.HOME ?? "/"`, `name: "xterm-256color"`,
      first.cols/rows. `p.onData(d => w.send(Buffer.from(d, "utf8")))` (binary), `w.on("message",
      (d, isBinary) => isBinary ? p.write(d.toString("utf8")) : handleControl(d))` where control
      `resize` → `p.resize`. `p.onExit(({exitCode}) => { w.send(JSON.stringify({exit: exitCode}));
      w.close(); })`. `w.on("close", () => p.kill("SIGHUP"))`.
- [ ] `spliceWorkspaceShell`: `const up = new WebSocket("ws://" + address + "/stream/pty")`; on
      `open` send `JSON.stringify(first)` then pipe: client binary/text → `up.send` unchanged; up
      binary/text → `w.send` unchanged (preserve `isBinary`); either `close`/`error` closes the
      other (`error` on `up` before `open` → `w.send({error: "workspace <address> did not
      answer"})`). Bound `open` to 5 s.
- [ ] Tests (`node --test`, real `node-pty`, `SHELL=/bin/sh`): (a) bench scope: open
      `ws://127.0.0.1:{port}/pty?scope=bench`, send resize, send `printf kl-%s ok\n`, expect a
      binary frame containing `kl-ok`; send `exit 4\n`, expect `{"exit":4}` then close; (b) the
      env inside the shell has no `KL_TOOL_TOKEN_FILE` (`echo ${KL_TOOL_TOKEN_FILE:-unset}` →
      `unset`) while the test process sets it; (c) workspace scope: a fake tool server (tiny `ws`
      server in the test that echoes binary frames upper-cased and answers `{"exit":0}` on the text
      frame `{"bye":1}`) at 127.0.0.1; `resolveTools` returns its address; the splice forwards the
      first resize verbatim, echoes bytes, forwards exit; (d) `scope=../x` → the upgrade fails
      (400); (e) resolve rejection → `{"error":…}` frame then close; (f) a bench socket closing
      kills the shell (`ps -p` of the child's pid, taken from `echo $$`, fails within 1 s).
- [ ] Gates: `npm run typecheck && npm run bench:test`. Commit: `Open a shell on the bench and
      splice one into a workspace's tool server`.

### Task 3: the desktop transport and the terminal panel

**Files**
- Modify: `harness/src/main.ts` (IPC), `harness/src/bench-client.ts` (a `pty(scope)` factory that
  returns a `WebSocket` through the tunnel, reusing `this.ws()`), `harness/src/preload.ts`
  (`window.harness.pty`), `harness/src/renderer/components/terminal/TerminalView.tsx`,
  `tabs.ts`, `TerminalPanel.tsx`, `harness/src/renderer/App.tsx` (`openShell` signature and the
  scope list), `harness/bench/test/desktop-*.test.ts` (whatever the renderer/tab tests are named:
  `renderer-rows.test.ts` style — add `desktop-terminal-tabs.test.ts` for `scopesOf`/`makeTab`).

**Interfaces consumed:** Task 2's `/pty?scope=`. **Produced:** `window.harness.pty = { open(id,
scope, cols, rows): Promise<void>; write(id, data: Uint8Array): void; resize(id, cols, rows): void;
close(id): void; onData(cb: (id, data: Uint8Array) => void): () => void; onExit(cb: (id, code:
number | undefined, error?: string) => void): () => void }`.

- [ ] `bench-client.ts`: `pty(scope: string): WebSocket` = `this.ws("/pty?scope=" +
      encodeURIComponent(scope))`. Throws `not connected` when no tunnel.
- [ ] `main.ts`: `const ptys = new Map<string, WebSocket>()`. `ipcMain.handle("pty:open", (e,
      id, scope, cols, rows))`: validate `id` (`/^t\d+$/`), `scope` (`bench` or the ws-id regex);
      open, `on("open")` → send `JSON.stringify({resize:{cols,rows}})`; `on("message", (d,
      isBinary))` → binary → `webContents.send("pty:data", id, new Uint8Array(d))`; text →
      parse; `exit` → `send("pty:exit", id, code)`; `error` → `send("pty:exit", id, undefined,
      msg)`; `on("close")` → if no exit was sent, `send("pty:exit", id, undefined, "disconnected")`;
      delete from map. `ipcMain.on("pty:write" | "pty:resize" | "pty:close")` act on the map;
      unknown id is ignored. On `auth` leaving `ready`, close every pty.
- [ ] `preload.ts`: expose exactly the `pty` surface above (`ipcRenderer.send` for write/resize/
      close, `invoke` for open, `on`/`removeListener` for the two events returning an unsubscribe).
- [ ] `tabs.ts`: `scopesOf(machine)` → `[{id:"bench", label:"bench", sub:"your machine in the
      team", kind:"bench"}, ...workspaces.map(w => ({id: w.id, label: w.name, sub: w.state ===
      "stopped" ? "stopped" : w.branch, kind:"workspace", disabled: w.state === "stopped"}))]`.
      `makeTab(machine, team: string, scopeId)` banner: `kloudlite shell · <bench|ws.name> ·
      <team>` and one dim line: bench → `your machine in the team; workspaces resolve by their tool
      servers`, workspace → `the workspace is the working directory`. Remove the `env` parameter
      (the caller passes the team name).
- [ ] `TerminalView.tsx`: delete the local echo/prompt code. `onMount`: `term.open`, `fit.fit()`,
      write banner, `harness.pty.open(tab.id, tab.scope, term.cols, term.rows)`; `term.onData(d =>
      harness.pty.write(tab.id, new TextEncoder().encode(d)))`; `term.onResize(({cols, rows}) =>
      harness.pty.resize(tab.id, cols, rows))`; a `ResizeObserver` on `host` → `fit.fit()`
      debounced 50 ms; subscribe `onData` (filter by id) → `term.write(data)`; `onExit` → write
      `\r\n\x1b[2m[process exited with code N]\x1b[0m` or `\r\n\x1b[2m[<error> — reopen the
      shell]\x1b[0m`, set a local `exited` signal, and on the next `Enter` keypress call
      `props.onClose()`. `onCleanup`: unsubscribe, `harness.pty.close(tab.id)`, `term.dispose()`.
      Theme effect unchanged.
- [ ] `TerminalPanel.tsx`: scope picker renders `disabled` scopes greyed and unclickable; each
      tab shows a small dot (`text-subtle`) when its view reported exit (lift `exited` per tab id
      into the panel through an `onExited(id)` prop, or a signal map — keep whichever is smaller).
      Pass `onClose` per view.
- [ ] `App.tsx`: `openShell(scopeId)` builds the tab with the current team's name (`teamName()`
      or the slug); the bench scope is always available when `benchState().connected`.
- [ ] Tests: `scopesOf` puts the bench first and disables stopped workspaces; `makeTab` banner
      text; main's `pty:open` id/scope validation (unit-test the validator function exported from
      a small `harness/src/pty-ipc.ts` if `main.ts` cannot be imported in tests — check how
      `desktop-controller.test.ts` tests main-side code and copy that shape).
- [ ] Gates: `npm run typecheck && npm run bench:test && npm run build`. Then run the app
      (`npx electron .` from `harness/`) against the owner's signed-in profile is NOT for the
      implementer — the controller does that. Commit: `Wire the desktop terminal to real shells
      on the bench and in workspaces`.

### Task 4: image, pod env, SLO ids

**Files**
- Modify: `deploy/bench/Dockerfile`, `crates/workspaces/src/k8s/bench.rs` (`SHELL=/bin/bash`
  env var), `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`, `bins/slo/src/stages/bench.rs`
  (two new steps), `web/apps/web/src/lib/fixtures/superadmin.ts` if the row-for-row test needs
  the two ids (run `cd web && bun test` to see).

- [ ] Dockerfile: in the `deps` stage, before `npm ci`, `RUN apt-get update && apt-get install -y
      --no-install-recommends python3 make g++ && rm -rf /var/lib/apt/lists/*` ONLY IF `npm ci`
      without it fails to produce `node_modules/node-pty/build/Release/pty.node` or a matching
      `prebuilds/linux-x64` — test both locally with `docker build --target deps` if docker is
      available; otherwise add the toolchain (safe, deps stage only) and say so in the report.
- [ ] `bench.rs`: add `var("SHELL", "/bin/bash")` beside `KL_TEAM`; update the k8s tests that
      snapshot the env list.
- [ ] Catalogue: two `Slo`s in the bench block of group 3, suite hourly, stage `14 · Experience`,
      feature `Benches`: `bench.shell.roundtrip` — sli "A shell opened on the bench through
      `/pty` echoes a marker and exits 0" — target `99.9 % ≤ 15000 ms`; `bench.shell.workspace` —
      sli "A shell opened through the bench into the run's workspace starts in the workspace
      directory" — target `99.9 % ≤ 20000 ms`. Mirror both rows in `deploy/slo.md` (the
      equality test tells you the exact format). Add both ids to `HOURLY_GROUPS[3]`.
- [ ] Probe steps (`bins/slo/src/stages/bench.rs`, after `bench.two_clients`): connect
      `ws://127.0.0.1:{port}/pty?scope=bench` through the existing tunnel port, send the text frame
      `{"resize":{"cols":100,"rows":30}}`, then the binary frame `printf 'kl-%s\\n' ok; exit 0\n`;
      collect binary frames until a text frame with `exit`; pass iff the bytes contain `kl-ok` and
      exit is 0, within the target. Workspace step: same with `scope=<the run's group-0
      workspace id>` — this runs in group 3 which has no workspace: instead move
      `bench.shell.workspace` to group 0 (beside `bench.workspace.tool_roundtrip`, same reason) and
      send `pwd; exit 0\n`, pass iff output contains the workspace root (`/workspace`, check
      `k8s::workspace.rs` for the mount path constant and use it). Skip both with the existing
      bench-not-up reason when the bench stage was skipped.
- [ ] Gates: `cargo test -p kloudlite-workspaces --lib slo`, `cargo test --manifest-path
      bins/slo/Cargo.toml`, workspace clippy, `cd web && bun test`. Commit: `Probe the bench and
      workspace shells hourly and build node-pty into the bench image`.

### Task 5: ship and verify (main session, not an implementer)

- [ ] Owner pushes `origin/desktop-login`; pod pull + `bash deploy/dev/ship.sh` (builds `kl`,
      bench image, slo image); pin; roll AKS + k3s (agent DaemonSet carries the new `kl`; bench
      pods pick the new image on their next wake — force the owner's bench by stopping it once).
- [ ] Hand-start an hourly; `bench.shell.roundtrip` and `bench.shell.workspace` pass.
- [ ] Owner opens a shell into the bench and into one workspace from the relaunched desktop app.

## Self-review

- Spec coverage: protocol (T1/T2), tool server route (T1), bench route + splice + token env
  drop + idle hold (T2), desktop IPC/xterm/scopes/exit text (T3), image + SHELL + SLO (T4),
  ship (T5). Out-of-scope items are not planned.
- Names consistent: `resize/cols/rows/exit/error/scope`, `pty:open/write/resize/close/data/exit`,
  `resolveFromApi`, `readFirstResize`, `attachBenchShell`, `spliceWorkspaceShell`.
- T1 and T2 touch disjoint trees and may run in parallel; T3 needs T2's route only by name and
  may start after T2's tests define the frames; T4 after T1–T3.
