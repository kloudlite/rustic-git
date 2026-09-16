# Terminals: tmux-backed, per-tab, reconnecting — implementation plan

> **For agentic workers:** implemented task by task by Opus implementers dispatched from the main
> session (Fable), which reviews each diff itself. Steps use `- [ ]` for tracking.

**Goal:** a terminal survives disconnects and pod restarts (tmux + saved state in the workspace
cache), belongs to the session tab it was opened from, reconnects on its own, and looks like a
native terminal (VS Code's standards). The `test>` prompt bug is found and fixed.

**Spec:** `docs/superpowers/specs/2026-09-17-terminals-tmux-and-tabs-design.md` — binding.

## Global constraints

- Worktree `/Volumes/kdisk/rustic-git-wt/desktop-login`, branch `desktop-login`. Prefix every
  shell command with `cd` there. Never `cargo` in `/Users/karthik/rustic-git`.
- Gates: Rust `cargo clippy --workspace --all-targets -- -D warnings` + touched crates' tests;
  harness `npm run typecheck && npm run bench:test && npm run build`.
- Wire names verbatim: query `session` (`[a-z0-9-]{1,48}`), route `GET /stream/pty/sessions`,
  tmux server `-L kl`, files `{ws}/.cache/tmux/{resurrect,history}`, `{ws}/.cache/zsh/history`.
- Session names come from the desktop only; the tool server validates and never invents one.
- No attribution lines in commits; imperative sentence case; do not push.

---

### Task 1: tmux in the workspace and the PTY route (Rust + image)

**Files:** `crates/ide/src/pty.rs` (+ `crates/ide/src/server.rs` route), `crates/workspaces/src/k8s/workspace.rs`
(`WS_BASE_PACKAGES` gains `tmux`, `tmux-resurrect` from the pin — verify attrs exist in the
pinned nixpkgs; `login_env` `HISTFILE` → `{ws}/.cache/zsh/history`; `XDG_CACHE_HOME` already
`{ws}/.cache`), `crates/workspaces/src/k8s/shell_rc.rs` (nothing new unless HISTFILE is echoed),
`Dockerfile` workspace stage (`/etc/tmux.conf` from `deploy/workspace-image/tmux.conf`, new),
`crates/ide/src/lib.rs` (`Config.cache_dir`), `bins/kl/src/main.rs` (`kl ide serve` restores at
start).

- [ ] `/stream/pty?session=<name>`: validate the name; spawn `tmux -L kl new-session -A -s
      <name> -x <cols> -y <rows>` (`-c` = workspace root) instead of the bare shell; `resize`
      still hits the PTY (`TIOCSWINSZ`, tmux follows with `aggressive-resize`). No `session` →
      today's shell. Tests: named → `tmux ls` shows it; reconnect with the same name attaches
      (two sequential sockets, the second sees the first's output via tmux redraw); name
      validation 400.
- [ ] `GET /stream/pty/sessions` → JSON `[{name, windows, attached, created}]` from `tmux -L kl
      ls -F`; empty list when no server. `DELETE /stream/pty/sessions/{name}` → `tmux -L kl
      kill-session -t <name>` (204; 404 when absent). Tests.
- [ ] `/etc/tmux.conf` per spec §1 + `run-shell` of tmux-resurrect with
      `@resurrect-dir {ws}/.cache/tmux/resurrect` (path via env `KL_WORKSPACE`), `@continuum-save-interval 1`,
      capture pane contents on. `kl ide serve` runs `tmux -L kl start-server \; run-shell
      <resurrect restore>` once at start if a save exists (log `tmux.restored {sessions}`);
      failures logged, never fatal.
- [ ] HISTFILE move + `mkdir -p {ws}/.cache/{zsh,tmux}` in the prelude (as user). Snapshot test
      updated deliberately.
- [ ] commit `Run every named terminal in tmux and keep its state in the workspace cache`

### Task 2: bench splice forwards the session name (harness/bench)

- [ ] `/pty?scope=…&session=<name>`: validate (same regex) and forward the query to the tool
      server (bench scope → `127.0.0.1:7788/stream/pty?session=`); `GET /pty/sessions?scope=` and
      `DELETE /pty/sessions/{name}?scope=` proxied to the tool server's routes. Tests with the fake
      tool server.
- [ ] commit `Forward terminal session names and listings through the bench`

### Task 3: desktop — per-tab terminals, reconnect, standards (harness/src)

**Files:** `main.ts` (`pty:open` gains `session`; `pty:sessions(scope)`), `preload.ts`,
`bench-client.ts` (`pty(scope, session)`, `ptySessions(scope)`), `renderer/components/terminal/*`,
`renderer/App.tsx`.

- [ ] `TermTab { id, label, scope, owner, session, banner }`; `makeTab(machine, team, ownerTabId,
      scopeId, n)` → `session = kl-<slug(owner)>-<n>`; drawer shows `tabs().filter(t => t.owner ===
      activeThreadId())`; scope picker removed — "+" opens in the active tab's scope (bench tab →
      `bench`, workspace tab → its id, ephemeral → its workspace); on first open of a tab, list
      `pty:sessions(scope)` and materialise tabs for existing `kl-<slug>-*` sessions.
- [ ] Reconnect per spec §3 (backoff 1/2/4/8 then 15 s while `benchState().connected`; banner
      `[disconnected — reconnecting…]`, cleared on data; after 5 min `press Enter to retry`);
      the tab's × sends `pty:kill(id)` → main calls `DELETE /pty/sessions/{name}?scope=` on the
      bench, which proxies `DELETE /stream/pty/sessions/{name}` on the tool server (`tmux -L kl
      kill-session -t <name>`; Tasks 1 and 2 add these two routes), then closes the socket.
- [ ] Tests: tab ownership filter, session naming, reconnect state machine (pure function with a
      fake clock), materialising from a listing.
- [ ] commit `Keep terminals per session tab and reattach them through tmux`

### Task 4: the prompt bug

- [ ] Reproduce on the fleet: `exec` in a workspace pod `env` vs the PTY's `env` (dial
      `/stream/pty` from the bench pod with the probe script in `scratchpad/ptyprobe.mjs`);
      find why starship did not run (`test>` = zsh default `%m>`… hostname?); fix in `pty.rs`
      env (e.g. `ZDOTDIR`, `STARSHIP_CONFIG`, `PATH`) or the rc; add a unit test on the spawned
      env and keep `bench.shell.workspace`'s `❯` assertion as the fleet check.
- [ ] commit `Give the PTY shell the same environment as an ssh login`

### Task 5: probes + ship

- [ ] `bench.shell.workspace` opens `?session=probe-<run>` twice and asserts the second attach
      sees the first's marker (reattach works); `ws.terminal.persists` (weekly): save → stop →
      start → `/stream/pty/sessions` lists the name again.
- [ ] Ship with the standing flow; owner verifies: close laptop lid 1 min, reopen → same shell.

## Order

T1 → T2 → T3 (T4 in parallel with T2/T3; it touches `pty.rs` env only — coordinate: T4 after T1
lands). T5 last.
