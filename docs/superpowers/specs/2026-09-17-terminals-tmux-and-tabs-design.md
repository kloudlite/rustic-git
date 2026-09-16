# Terminals: tmux-backed, per-tab, reconnecting

Owner asks (2026-09-16 late): terminals flicker and do not feel native; a dropped connection kills the
shell ("when I tried to connect it should reconnect"); "each terminal open belongs to that tab …
terminals will belong to that session workspace"; "better use tmux as we need to show across the
different sessions"; "history etc are not working"; "check the standards".

## What exists

`kl ide serve` `/stream/pty` forks a login shell per socket (one socket, one shell); the bench
splices `/pty?scope=` to it; the desktop's `TerminalView` (xterm.js 6 + WebGL) holds one socket
per tab and prints `[disconnected — reopen the shell]` when it drops. Terminal tabs are global to
the window. zsh saves no history (no `HISTSIZE`/`SAVEHIST`; fixed in the quick-fix wave). The
screenshot of 2026-09-16 23:50 shows a `test>` prompt with blank lines between prompts, i.e. a
shell that did not run starship and printed the default zsh prompt — cause to be found (likely the
PTY's login shell not reading `/etc/zshrc` because `ZDOTDIR`/`ENV` differ between the ssh path
and the PTY path, or `RPROMPT`/`line_break` from starship.toml without `$directory` content).

## Design

### 1. Every terminal is a tmux window

The tool server's `/stream/pty` takes `?session=<name>` (`[a-z0-9-]{1,48}`). It runs
`tmux -L kl new-session -A -s <name> -x <cols> -y <rows>` under the PTY: `-A` attaches when the
session exists, so a reconnect with the same name lands in the same shell with its scrollback
redrawn by tmux itself. The socket closing detaches the client; the session lives on until the
shell exits or `tmux kill-session`. `tmux` comes from the Nix profile (`WS_BASE_PACKAGES` gains
`tmux`), with a shipped `/etc/tmux.conf`: `set -g default-terminal tmux-256color`, `set -g
mouse on`, `set -g history-limit 50000`, `set -g status off` (the desktop draws tabs; tmux's bar
would be a second one), `set -g escape-time 0`, `set -g focus-events on`, `setw -g
aggressive-resize on`, `set -g destroy-unattached off`, `set -g exit-empty on`. Without a name
(`/stream/pty` alone) the route keeps today's one-shot shell for `exec`-style callers and the
probe. `GET /stream/pty/sessions` lists `tmux -L kl ls -F '#{session_name} #{session_windows}
#{session_attached} #{session_created}'` so another device can show and reattach the same
terminals.

Bench scope: `/pty?scope=bench&session=<name>` splices to the pod's own tool server with the
same query; workspace scope forwards it unchanged.

### 2. Terminals belong to a tab

A `TermTab` gains `owner: string` — the session-tab id it was opened from (bench thread,
workspace thread, ephemeral). The drawer shows the active pane's active tab's terminals only;
all stay mounted (display:none) so switching never drops a socket. "New terminal" in a tab opens
in that tab's scope: a workspace tab → that workspace, the bench tab → the bench; the scope
picker is gone (a scope is the tab). The tmux session name is `kl-<owner-slug>-<n>`, so the same
tab on another device reattaches the same shells; on open the desktop lists `/stream/pty/sessions`
for the scope and pre-populates the tab's terminals from what exists.

### 2b. Tabs are the sessions (owner, 2026-09-17 04:30 IST)

The tabs of a session tab are a one-to-one mirror of the tmux sessions named `kl-<slug>-*` for
that scope, both ways: the desktop reconciles against `GET /pty/sessions` every 5 s (and on tab
switch and reconnect) — a session that appeared elsewhere gets a tab, a session that ended loses
its tab, a shell that exits closes its tab at once, and a tab's × kills its session. Never two tabs
for one session. A tab younger than 10 s is spared one reconcile, since its session may not be
listed yet.

### 3. Reconnect

On a socket drop the view prints `[disconnected — reconnecting…]` and retries `pty:open` with
the same session name at 1, 2, 4, 8 s, then every 15 s, while the bench-client reports connected
(a bench asleep is waited for, not retried against). Success redraws via tmux; the banner clears.
After 5 minutes of failures the line becomes `[disconnected — press Enter to retry]`. The tab's
× runs `tmux kill-session -t <name>` through the same socket first (a killed shell is the
person's choice; a dropped socket never is).

### 4. Standards

xterm options as VS Code ships them (Menlo/SF Mono, 13 px, line height 1.2, blinking bar cursor,
Option = Meta, 10 000 lines scrollback, no write coalescing — done in the quick-fix wave). Local
echo typeahead is deferred: with tmux the perceived latency is one round trip per keystroke
(~60–120 ms through the tunnel); if it still feels wrong after this lands, VS Code's
`TypeAheadAddon` approach is the next step, documented here as the ceiling.

### 5. Prompt bug

Find why the PTY shell printed `test>`: compare `env` inside `/stream/pty` against an ssh login
(`ZDOTDIR`, `ENV`, `TERM`, `STARSHIP_CONFIG`, `PATH` to starship); the fix is in
`crates/ide/src/pty.rs` env or `shell_rc`. A probe assertion already looks for `❯`.

### 6. Where a terminal lives (owner ruling, 2026-09-16 23:58 IST: "persist in that workspace
working directory … use that cache")

Live state is the tmux server inside the workspace container (`tmux -L kl`, socket under the pod's
`/tmp`): it survives the laptop closing, the tunnel dropping, the desktop restarting, another device
attaching, and the bench sleeping. Saved state lives in the WORKSPACE at `{ws}/.cache/tmux/`
(`resurrect/` — tmux-resurrect's save files — and `history/`), so it is snapshotted, replicated,
cloned and restored with the workspace like every other cache (the caches design: everything that
can be cached goes in the workspace folder). `tmux-resurrect` (from the Nix profile, pinned) saves
every 60 s (`tmux-continuum` interval) and on detach, and `kl ide serve` runs `restore` once at
start before serving `/stream/pty`, so after a pod stop, a move, or a clone the same terminal
names come back with their layout, cwd and scrollback text; the running processes do not (stated
ceiling, no tool can). The zsh `HISTFILE` also moves into `{ws}/.cache/zsh/history` per this ruling
(history belongs to the work, not the node), replacing the homecache location.

## Out of scope

Typeahead (ceiling above). Sharing a tmux session between two people. Terminals in environments. Restoring running processes after a pod restart.

## Security

No new credential; tmux runs as uid 1000 inside the workspace container behind the same fence
as `exec`. Session names are validated; the list route is on the same fenced port.
