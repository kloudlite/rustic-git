# `kl ide serve` — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or
> superpowers:executing-plans, task by task. All edits in the dev pod (`/work/src`); build, test,
> commit and push from there. Never edit while `ship` is building.

**Goal:** a tool server inside every workspace pod — `kl ide serve` — exposing read/write/edit/
glob/grep/exec/process/watch and graft over MCP on `127.0.0.1:7788`, reached through the ssh
tunnel a person already holds.

**Architecture:** a new library crate `crates/ide` (`kloudlite-ide`: axum HTTP + WebSocket,
hand-rolled MCP JSON-RPC, tool modules) driven by a new `ide serve` subcommand in `bins/kl`. The
workspace image gains node + graft and the prelude starts the server as `kl` before sshd. Graft's
own MCP server runs as a child on stdio and is proxied; `build`/`blast` wrap the CLI.

**Spec:** `docs/superpowers/specs/2026-09-11-kl-ide-serve-design.md`.

**Decisions taken (defaults, owner did not object):** MCP + two WebSocket streams; phase 1 reach
= ssh tunnel only; the server lives in `kl` (musl). No PTY in the first cut (`pty: true` answers
`unsupported`); graft proxied from `@nanonets/graft@0.18`.

## Global constraints

- `kl` stays a static musl binary: every new dependency must build for `x86_64-unknown-linux-musl`
  with no C toolchain surprises (`notify`, `regex`, `globset`, `ignore`, `tokio-tungstenite` with
  rustls or no TLS — the server is plaintext loopback).
- Bind `127.0.0.1:7788` only. Refuse to start unless uid is `kl` (1000), `$HOME` is `/home/kl`,
  `$KL_WORKSPACE` is set and exists, and `~/.config/git/ignore` carries the marker line
  `# kloudlite: derived state the platform places inside a workspace directory`.
- Every path is canonicalised and must stay under `/home/kl`; otherwise the tool answers an
  MCP error `EACCES <path>`.
- Limits: read/write ≤ 10 MiB, grep ≤ 2 000 matches, glob ≤ 5 000 paths, process ring buffer
  4 MiB, ≤ 32 processes, ≤ 32 watches, exec timeout default 120 s max 600 s.
- Log one `ide.call` line per tool call (tool, ms, ok, bytes) and `ide.process` on start/exit, via
  `tracing` to stderr in the same JSON shape every other binary uses (`kloudlite_core::log`).
- Tool names in MCP: exactly `read write edit glob grep exec process_list process_output
  process_write process_kill watch watch_poll watch_stop graft_find_code graft_find_all
  graft_trace_calls graft_file_api graft_repo_map graft_check_freshness graft_build graft_blast`.
- `cargo clippy --workspace --all-targets -- -D warnings` clean; commits imperative sentence case.

---

### Task 0: Commit spec and plan

**Files:** Create `docs/superpowers/specs/2026-09-11-kl-ide-serve-design.md` (from
`/tmp/kl-ide-serve-spec.md`), `docs/superpowers/plans/2026-09-11-kl-ide-serve.md` (this file).

- [ ] `kubectl cp` both into the pod; strip the "Open decisions" section from the spec and replace
  it with "Decisions: MCP + two streams; ssh tunnel first; grow `kl`; no PTY in v1; graft proxied."
- [ ] Commit: `Spec and plan: kl ide serve, the workspace tool server`.

### Task 1: Crate skeleton, `kl ide serve`, `/healthz`

**Files:** Create `crates/ide/Cargo.toml`, `crates/ide/src/lib.rs`, `crates/ide/src/server.rs`,
`crates/ide/src/guard.rs`; Modify `Cargo.toml` (workspace members + `[workspace.dependencies]`
for `notify = "7"`, `globset = "0.4"`, `ignore = "0.4"`, `regex = "1"`), `bins/kl/Cargo.toml`,
`bins/kl/src/main.rs`.

**Interfaces produced:** `kloudlite_ide::serve(cfg: Config) -> anyhow::Result<()>`,
`Config { bind: SocketAddr, root: PathBuf, home: PathBuf, graft_dir: Option<PathBuf> }`,
`guard::preflight(&Config) -> Result<(), String>`.

- [ ] `crates/ide/Cargo.toml`: `tokio`, `axum` (features `ws`, `json`), `serde`, `serde_json`,
  `anyhow`, `tracing`, `futures`, `bytes` (workspace = true), plus the four new ones.
- [ ] **Failing test** `crates/ide/src/guard.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preflight_names_every_missing_precondition() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config { bind: "127.0.0.1:0".parse().unwrap(), root: tmp.path().join("ws"), home: tmp.path().to_path_buf(), graft_dir: None };
        let why = preflight_with(&cfg, 1000, 1000).unwrap_err();
        assert!(why.contains("workspace dir"), "{why}");
        std::fs::create_dir_all(&cfg.root).unwrap();
        let why = preflight_with(&cfg, 1000, 1000).unwrap_err();
        assert!(why.contains("gitignore"), "{why}");
        std::fs::create_dir_all(tmp.path().join(".config/git")).unwrap();
        std::fs::write(tmp.path().join(".config/git/ignore"), "# kloudlite: derived state the platform places inside a workspace directory\n.cache/\n").unwrap();
        assert!(preflight_with(&cfg, 1000, 1000).is_ok());
        assert!(preflight_with(&cfg, 0, 1000).unwrap_err().contains("uid"));
    }
}
```
- [ ] Implement `preflight(cfg)` = `preflight_with(cfg, libc::geteuid(), 1000)`: checks uid ==
  expected, `cfg.root.is_dir()`, `cfg.home.join(".config/git/ignore")` contains the marker.
  (`libc` is already a workspace dep.)
- [ ] `server.rs`: `axum::Router` with `GET /healthz` →
  `{"ok":true,"root":"…","graph":"unknown"}` (graph filled in Task 5); `serve()` binds `cfg.bind`.
- [ ] `bins/kl/src/main.rs`: add
```rust
    /// The workspace tool server: MCP on 127.0.0.1:7788 for sessions outside the pod
    Ide {
        #[command(subcommand)]
        cmd: IdeCmd,
    },
// …
#[derive(Subcommand)]
enum IdeCmd {
    /// Serve read/write/edit/glob/grep/exec/watch and graft over MCP on loopback
    Serve {
        #[arg(long, default_value = "127.0.0.1:7788")]
        bind: std::net::SocketAddr,
        #[arg(long)]
        graft_dir: Option<std::path::PathBuf>,
    },
}
```
  and in `real_main`: `Cmd::Ide { cmd: IdeCmd::Serve { bind, graft_dir } }` builds `Config` from
  `KL_WORKSPACE` and `HOME`, runs `preflight`, then a `tokio` runtime `block_on(kloudlite_ide::serve(cfg))`.
  `kl` gains `tokio` (rt-multi-thread, macros) and `kloudlite-ide` deps. Update `kl`'s `//!` doc:
  three verbs now.
- [ ] `cargo build -p kl --target x86_64-unknown-linux-musl` in the pod → must succeed.
- [ ] Test: `cargo test -p kloudlite-ide`. Commit: `kl ide serve: crate skeleton, preflight, healthz`.

### Task 2: MCP core and the file tools

**Files:** Create `crates/ide/src/mcp.rs`, `crates/ide/src/tools/mod.rs`,
`crates/ide/src/tools/files.rs`, `crates/ide/src/paths.rs`; Modify `server.rs`.

**Interfaces produced:**
```rust
pub struct Tool { pub name: &'static str, pub description: &'static str, pub schema: serde_json::Value }
pub trait ToolSet: Send + Sync { fn tools(&self) -> Vec<Tool>; fn call<'a>(&'a self, name: &str, args: serde_json::Value) -> BoxFuture<'a, Result<serde_json::Value, ToolError>>; }
pub enum ToolError { Unknown(String), Invalid(String), Denied(String), Failed(String) }
pub fn confine(root: &Path, home: &Path, given: &str) -> Result<PathBuf, ToolError>  // paths.rs
```
- [ ] **Failing tests** `paths.rs`: `confine` resolves relative to root, accepts absolute under
  home, rejects `../..` escapes and symlinks that leave home (create one in a tempdir), returns
  `Denied` naming the path.
- [ ] `mcp.rs`: JSON-RPC 2.0 over `POST /mcp` (streamable HTTP, JSON response, no SSE in v1):
  methods `initialize` (answers `protocolVersion: "2025-03-26"`, `capabilities: {tools: {}}`,
  `serverInfo {name: "kl-ide", version}`), `notifications/initialized` (204), `tools/list`,
  `tools/call` (result `{content: [{type: "text", text}], isError}`), `ping`. Unknown method →
  `-32601`. A `ToolError` becomes `isError: true` with the message as text. Test with
  `tower::ServiceExt::oneshot`: initialize → tools/list names the file tools → tools/call read.
- [ ] `tools/files.rs` implementing the spec table: `read` (numbered lines, `offset`, `limit`,
  `total_lines`, `truncated`; binary → `{mime, size}` via a NUL-byte sniff of the first 8 KiB),
  `write` (temp + rename, `create_dir_all`, `bytes`, `created`), `edit` (`edits[]`, all-or-nothing
  on an in-memory copy, `old` must occur exactly once unless `replace_all`, error names the edit
  index), `glob` (`ignore::WalkBuilder` + `globset`, gitignore-aware, sorted by mtime desc, cap
  5 000), `grep` (`regex` over `ignore::Walk`, `mode` files|content|count, `context`, `glob`,
  cap 2 000; a `rg` binary on PATH is NOT used — one implementation, testable).
- [ ] Tests per tool in `files.rs` against a tempdir root: read offsets and binary sniff; write
  creates parents and is atomic (no partial file on a failed rename — simulate with a read-only
  parent); edit uniqueness and index in the error; glob respects `.gitignore`; grep modes.
- [ ] Wire `ToolSet` registry into `server.rs` (`/mcp` handler holds `Arc<Vec<Box<dyn ToolSet>>>`).
- [ ] Commit: `kl ide serve: MCP endpoint and the file tools`.

### Task 3: exec, processes, and the process stream

**Files:** Create `crates/ide/src/tools/exec.rs`, `crates/ide/src/procs.rs`,
`crates/ide/src/stream.rs`; Modify `server.rs`.

**Interfaces produced:**
```rust
pub struct Procs { /* Mutex<HashMap<String, Proc>> */ }
pub struct Proc { pub id: String, pub cmd: String, pub started_at: String, pub state: State, pub exit_code: Option<i32>, out: Ring, err: Ring, stdin: Option<ChildStdin>, tx: broadcast::Sender<Frame> }
pub enum Frame { Stdout(Bytes), Stderr(Bytes), Exit(i32) }
pub struct Ring { buf: VecDeque<u8>, cap: usize, dropped: u64, start: u64 }  // start = absolute offset of buf[0]
```
- [ ] **Failing tests** `procs.rs`: `Ring` keeps the last `cap` bytes and reports `dropped`;
  `read_since(offset)` returns bytes from an absolute offset and the `next` offset; a `Proc` that
  exits records `exit_code` and broadcasts `Exit`.
- [ ] `exec` tool: `cmd` (string → `sh -lc`, array → argv), `cwd` (confined, default root), `env`
  (merged over the process env), `timeout_ms`, `detach`. Job: wait with timeout, kill on timeout
  (TERM then KILL after 5 s), answer `{exit_code, stdout, stderr, truncated, timed_out}`. Detached:
  register in `Procs`, answer `{id}`. Both spawn as the current user with the login env inherited;
  process group per child (`pre_exec` setsid) so kill reaches children.
- [ ] `process_list`, `process_output {id, since}`, `process_write {id, data}`, `process_kill {id, signal}`
  as in the spec; a finished process stays listed for 10 min then is reaped.
- [ ] `stream.rs`: `GET /stream/process/{id}` WebSocket (axum `ws`), replays the ring from
  offset 0 then follows `broadcast` frames; text frames `{"stream":"stdout","data":"…"}` with
  lossy UTF-8, final `{"exit":code}`.
- [ ] Tests: exec `echo hi` job; a detached `sh -c 'for i in 1 2 3; do echo $i; sleep 0.1; done'`
  polled to completion; kill of a `sleep 30`; timeout of `sleep 5` with `timeout_ms: 200`.
- [ ] Commit: `kl ide serve: exec jobs, detached processes, and the process stream`.

### Task 4: watch

**Files:** Create `crates/ide/src/tools/watch.rs`, `crates/ide/src/watches.rs`; Modify
`stream.rs`, `server.rs`.

- [ ] `watch {paths[] | cmd, pattern?, once?}` → `{id}`: paths use `notify::RecommendedWatcher`
  (recursive, debounced 100 ms, events `{path, kind}`); `cmd` runs a detached process and emits
  `{line}` for each output line matching `pattern` (whole line when absent); `once` stops the
  watch at the first event. `watch_poll {id, since}`, `watch_stop {id}`. Ring of the last 1 000
  events. `GET /stream/watch/{id}` mirrors the process stream.
- [ ] Tests: a file created under a watched dir arrives as an event within 1 s; a `cmd` watch on
  `printf 'a\nready\n'` with pattern `ready` and `once` ends `stopped` with one event.
- [ ] Commit: `kl ide serve: watch tools and stream`.

### Task 5: graft — proxy, build, blast, freshness

**Files:** Create `crates/ide/src/graft.rs`, `crates/ide/src/tools/graft.rs`; Modify
`tools/files.rs` (post-edit hook), `server.rs` (healthz `graph`).

**Interfaces produced:** `Graft::spawn(root, graft_dir) -> Graft` (child `graft mcp` on stdio,
`DO_NOT_TRACK=1`, restarted on exit), `Graft::call(name, args) -> Result<Value>`,
`Graft::refresh(paths: &[PathBuf])` (runs `graft build <root>` — cache reuse makes it
incremental — serialised, debounced 2 s), `Graft::state() -> Ready | Building | Drifted | Absent`.
- [ ] Proxy: `initialize` once against the child, then forward `tools/call` for the six names;
  `tools/list` is served from a fixed table (schemas copied from graft 0.18's list, as fetched in
  this session) so the server answers even while graft is starting.
- [ ] `graft_build {deep?, no_reuse?}` runs `graft build [--deep] [--no-reuse]` as a detached
  process through `Procs`, answers `{id}`; `graft_blast {base?, depth?}` runs
  `graft blast --format json [--base B] [--depth D]` as a job and answers the parsed JSON.
- [ ] Freshness: `write`, `edit` and every job `exec` call `Graft::refresh` after completion
  (fire and forget); a `notify` watcher on root (excluding `graft/`, `.git/`, `.cache/`,
  `node_modules/`, `target/`) calls it too, debounced. On start: `graft/` absent → `graft build`
  (state `Building`), else `graft check --json` → `Drifted` → refresh → `Ready`. `/healthz`
  reports the state.
- [ ] `--deep` answers `no_provider` unless `GRAFT_PROVIDER` and `GRAFT_API_KEY` are set.
- [ ] Tests (need `graft` on PATH: `#[ignore]`d unit tests run in the pod where it is installed,
  plus one unconditional test that the table has exactly eight `graft_*` names and every schema
  has `type: object`).
- [ ] Commit: `kl ide serve: graft proxied, built and kept fresh from the server's own tool calls`.

### Task 6: Image, prelude, kl-connect tunnel

**Files:** Modify `Dockerfile` (workspace stage), `crates/workspaces/src/k8s/workspace.rs`
(`prelude`), `crates/workspaces/src/k8s/tests/pod.rs`, `bins/kl-connect/src/main.rs`,
`bins/kl-connect/src/ws.rs`.

- [ ] Dockerfile workspace stage: `apk add --no-cache nodejs npm` and
  `RUN npm install -g @nanonets/graft@0.18.0 && npm cache clean --force`; `ENV DO_NOT_TRACK=1`.
- [ ] `prelude`: before `exec … sshd`, as `kl`:
```sh
su {SSH_USER} -s /bin/sh -c 'cd {workspace_dir} && KL_WORKSPACE={workspace_dir} exec kl ide serve >> /home/kl/.local/state/kl-ide.log 2>&1' &
```
  (env is the pod env; `kl` is on PATH). Test: `prelude("api")` contains `kl ide serve` and it
  runs before the `exec` line.
- [ ] `kl-connect ws ide <target> [--port 7788]`: `ssh -N -L <port>:127.0.0.1:7788 <host>` through
  the existing ProxyCommand, printing the `claude mcp add --transport http workspace
  http://localhost:<port>/mcp` line once connected. Reuse `WsCmd::Ssh`'s session/proxy code.
- [ ] Commit: `Workspace image runs kl ide serve; kl-connect ws ide opens the tunnel`.

### Task 7: Probe ids

**Files:** Modify `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`,
`web/apps/web/src/lib/fixtures/superadmin.ts`, `bins/slo/src/stages/experience.rs`,
`bins/slo/src/stages/experience_ws.rs`.

- [ ] Rows (hourly, `14 · Experience`): `ide.serve.up` — "kl ide serve inside a fresh workspace
  answers /healthz within 2 s of ready", `p95(240_000)`; `ide.exec` — "an MCP tools/call exec of
  `true` through the workspace's own server answers exit 0", `avail(99.9)`. Ids are `[a-z0-9.]`.
- [ ] Steps in `experience_ws.rs`: `create`, then `ws_exec` of
  `curl -sf http://127.0.0.1:7788/healthz` (retry 2 s up to 60 s), then `ws_exec` of a
  `curl -s -X POST http://127.0.0.1:7788/mcp -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"exec","arguments":{"cmd":"true"}}}'`
  asserting `"exit_code":0` in the text; `drop_ws`. Wire into `experience.rs` IDs and dispatch.
- [ ] Commit: `Probe: the workspace tool server is up and answers an exec`.

### Task 8: Docs, ship, roll, verify

**Files:** Create `docs/product/agent-tools/ide-server.md`; Modify `web/apps/web/src/lib/docs.ts`
(NAV: `agent-tools/ide-server` after `agent-tools/exec`), `docs/product/human-tools/kl-connect.md`
(`ws ide`), `docs/product/reference/cli/kl.md` and `kl-connect.md`, `CLAUDE.md` (a paragraph
under "Workspaces and environments": what the server is, loopback + tunnel, graft freshness, the
probe ids).
- [ ] `ide-server.md`: what it is, connecting (`kl-connect ws ide api` then the `claude mcp add`
  line), the tool table from the spec, limits, graft section, what is not there (PTY, gateway).
- [ ] Ship (`deploy/dev/ship.sh`), pin, commit, push origin + platform, `deploy/roll.sh`, k3s apply.
- [ ] Verify directly (as in the caches change): create a workspace via `/v1`, `kubectl exec`
  `curl /healthz` → `graph: ready|building`, `tools/list` has 21 names, `read` of a file, `exec
  true`, `graft_find_code` answers after the build; from the laptop: `kl-connect ws ide` → `claude
  mcp add` → one `mcp__workspace__read` call from a Claude Code session. Then run
  `deploy/dev/run-job.sh hourly`.
- [ ] Memory note with what the fleet found.

## Self-review

- Spec coverage: placement (T1, T6), protocol (T2, T3, T4), tools table (T2–T4), graft incl.
  freshness and global ignore precondition (T5, T1 preflight), observability (`ide.call` in T2's
  registry wrapper — add it there), binary (T1), probe (T7), docs (T8). PTY: deferred by decision.
- Placeholders: none; each task names files, tests and the exact behaviour.
- Types: `ToolError`, `ToolSet`, `Procs`, `Frame`, `Graft` named once each and reused as spelled.
