# TUI state sync: the daemon owns every session's state

Date: 2026-10-09. Status: approved; §8 grants and §12 amended by the person the same day.

## What the person asked for

- "Check for any such issues. We need to have services, other sessions, workspaces, agents all in
  sync." (After `/clear` in one TUI left the old transcript on screen in another.)
- Earlier, still binding: "I just want the sessions and their states to be in sync. No need to sync
  stroke by stroke."

Success: two TUIs on one bench (laptop `kl-tui` and the ttyd TUI, in any mix) never disagree about
anything a session or the space holds. Whatever one does (prompt, answer a card, switch model,
clear, toggle codemode, log in, create or delete a workspace), the other shows it without the
person touching it, and nothing one TUI does is undone by the other reopening. What each TUI shows
(focus, scroll, drafts, vim, theme, sidebar width) stays its own.

## Today, and why it drifts

The agents live in the bench daemon (`packages/backend/src/daemon.ts`, one `LocalBackend`, one
`serve()` per connection). The daemon already pushes session events to every view of a key and
the session list (`sessions.watch`, with live `busy`). Everything else is held in each TUI and
only ever flows from TUI to daemon:

| State | Held by | Drift |
| --- | --- | --- |
| model, thinking level, codemode of a live session | each TUI (`sessions[key].model`, `prefs`) | a reopen sends the TUI's stale values; `LocalBackend.session` applies them (`local.ts` shared-open path), undoing the other TUI's `/model` |
| defaults (thinking, autoCompact, codemode, defaultModel) | each TUI, read once from `hello` | never refreshed |
| queue, token count | each TUI | queue only via live `queue_update`; tokens summed per TUI, nothing at open |
| names, descriptions | each TUI (`sessionNames`, `sessionDescs`) | auto-title from a stale map overwrites a name |
| permission and question cards | the newest client of the key (`clients.ts` `route`) | the other TUI never sees the card the turn is blocked on; abort never withdraws it |
| permission mode | each TUI | the newest client decides for everyone, invisibly |
| user messages | only the TUI that sent them | `handleAgentEvent` renders assistant messages only |
| the agent after a rebuild | the TUI that caused it | others get `session_closed`, drop the handle, go deaf until they prompt |
| auth, model catalog | each TUI, at mount and own login | another TUI's `/login` is invisible |
| workspaces, environments, services, processes, tasks | each TUI, polling `space()` every 5 s | an error empties the list and kicks focus to main |
| files view | each TUI, bumped on its own edit events | another session's edit never refreshes it |

The rule this spec applies everywhere: **the daemon holds the state and pushes it; a TUI renders
what it was pushed and sends only intents.** No TUI sends state it merely remembers.

## Design

### 1. Protocol 2

`PROTOCOL` becomes 2 (`wire.ts`). A 1-vs-2 mix fails at `hello` as it does today (exit 3 path in
`kl-connect`). Dev phase: no compatibility shim, no fallback for a protocol-1 bench.

### 2. Session state, pushed

New event on the existing `session` channel, to every view of the key:

```ts
type SessionState = {
  type: "session_state";
  model: ModelRef;            // what the agent runs now
  thinkingLevel: ThinkingLevel;
  autoCompact: boolean;
  codemode: boolean;
  queued: { steering: string[]; followUp: string[] };
  tokens: number;             // the counter the TUI keeps today, moved here; 0 after clear
};
```

The daemon keeps one `SessionState` per live key, updates it on `setModel`, `setThinkingLevel`,
`setAutoCompactionEnabled`, `queue_update`, assistant `message_end` usage, and build, and emits it
after each change. `session.open` returns it beside `messages` and `busy`. The TUI deletes its own
writes of these fields and renders from the pushed state; `queue_update` stays as the event that
moves the queue, the state carries it at open.

### 3. Opens carry no state

`session.open` sends `initial: { model, thinkingLevel, autoCompact, codemode }`, used only when the
daemon has no live agent for the key and builds one. The shared-open path in
`LocalBackend.session` no longer applies the opener's model or thinking level. Rebuild on open stays
only for a missing tool (`built.tools`).

Every change is an explicit call on the handle: `setModel`, `setThinkingLevel`,
`setAutoCompactionEnabled`, and a new `setCodemode(on)`.

### 4. Rebuilds happen inside the daemon, and views reconnect

Today the TUI rebuilds (`reopen`) for a Claude/pi model switch, a codemode toggle and a login. That
moves into the daemon:

- `setModel` across the anthropic/pi boundary and `setCodemode` rebuild the agent in the daemon
  (refused with `"a turn is running"` while busy, as the TUI refuses today).
- Codemode is also a default: `settings.write({ codemode })` rebuilds every idle live agent and
  marks busy ones for their `agent_end` (the existing `#rebuild` set).
- Login: after `auth.login` succeeds the daemon rebuilds idle Claude agents (what the TUI's login
  reopen does now).

`session_closed` gains a reason: `{ type: "session_closed", reopen: boolean }`. `reopen: true` for
a rebuild or `/clear`; `false` for idle dispose and workspace delete. On `reopen: true` a TUI that
holds the key reopens it at once (`ensureAgent`) and replaces its transcript from the reply. This
is what makes another TUI's `/clear` empty this one; the `cleared` mark from commit 35cbe231 is
removed as redundant.

`LocalBackend.session` is serialised per key (a promise chain in a `Map<string, Promise>`): two
opens racing a rebuild (A's build awaiting `createSession`, B's reopen arriving) must find one
agent, never build two. Two agents on one session file is the one-writer rule broken.

### 5. Transcript open is a replace, with the live ids

On every open the TUI replaces the key's entries from `messages`, not only when empty. Rows get the
ids the live path uses (`m${timestamp}` for assistant text and thinking, `toolCallId` for tools), so
a live event arriving after the snapshot updates a row instead of adding a second. A tool call with
no result yet and `busy` true is `running`, not `ok`. Prompt history and draft are kept.

### 6. User messages render from events

`handleAgentEvent` renders `message_start` with `role: "user"` as a user row. The daemon decorates
that event with `shown`: the person's text without the role card the first prompt carries
(`roleCard` in `local.ts`), and strips it the same way in the `messages` snapshot. The TUI stops
appending a user row itself on send; the row arrives from the event, so the sending TUI and every
other TUI show the same row at the same point. Steers and followUps appear when the agent takes
them, `[from ws]` replies appear when delivered.

Id: `u${timestamp}`, so a snapshot and the live event agree.

### 7. Names and descriptions from the list

The TUI drops `sessionNames` and `sessionDescs` and reads name and description from the watched
list. Auto-title moves to the daemon: a client-typed prompt to a key with no name names it (first
40 characters, as today), inside `#attach`'s typed `prompt`.

### 8. Cards are the daemon's

Permission and question cards stop being requests to the newest client.

- **Mode in the daemon, grants in the TUI.** Permission mode (`default`, `acceptEdits`, `plan`,
  `bypass`) moves into `LocalBackend` with the `gate` logic from `app.tsx`. One mode for the
  daemon: shift+tab in any TUI changes it for all, pushed as a `perm` event `{ mode }`.
  Always-allow stays in each TUI process (the person: "Let always allow be in Tui"): the daemon
  never learns of a grant. Choosing "Allow always" on a card answers the ask `once` and records the
  tool for the asking key in that TUI's `alwaysAllow`. When an `ask` arrives for a tool that TUI
  has granted for that key, the TUI answers it `once` at once, without showing a card. A grant in
  one TUI therefore answers for everyone while that TUI is connected, and is gone when it quits.
- **Asks.** A gated call that needs a person, and the `question` tool (moved from the TUI into the
  daemon's registry), create an ask `{ id, key, kind: "permission" | "question", tool, title,
  subtitle, body, diff, options }` (`key` is the asking key: a delegated session's caller). The daemon emits `ask` on a broadcast channel to every connection. Each
  TUI keeps the list and shows a card only for its active key, as `askFor` does now; an ask for
  another key shows as a mark on that session in the sidebar.
- **Answer.** `ask.answer { id, choice }`. First answer wins; the daemon emits `ask_resolved { id }`
  to every connection and each TUI drops the card. A late answer is ignored.
- **Withdraw.** The gate's abort signal, the agent's dispose and a workspace delete resolve the
  ask (as reject for a permission, as an error string for a question) and emit `ask_resolved`.
- **Reconnect.** `hello` returns pending asks and the mode, so a TUI that connects mid-turn shows
  the card the turn is waiting on.
- With no TUI connected the ask waits, as today.

`clients.ts` keeps routing only the TUI-owned tools that remain; if none remain after `question`
moves, it is deleted.

### 9. Settings and auth, pushed

- `settings.write` in the daemon emits `settings { settings }` to every connection. A TUI applies
  the session defaults (`thinkingLevel`, `autoCompact`, `codemode`, `defaultModel`) to its footer
  for keys with no live agent. View preferences (vim, theme, sidebar width) are read once at boot
  and never applied from a push: they are per TUI.
- A finished `auth.login` emits `auth_changed`. Every TUI refetches providers and the catalog
  (`models.refresh`), as the logging-in TUI does now.

### 10. The space, pushed

- `space.watch` (a stream, like `sessions.watch`): the daemon polls `space()` every 5 s, once for
  the bench instead of once per TUI, and also right after every `agent_end` and every
  `tool_execution_end` of a platform tool in any session. It emits only when the JSON differs.
- A failed poll emits the last good view with `error` set. The TUI shows the error and keeps the
  list and focus. Focus moves only when the workspace it is on is really gone from a good view.
- The TUI deletes its 5 s interval and its post-turn `refresh`.
- `forgetSessions` (workspace delete) calls the sessions watchers, so the list drops the workspace's
  sessions at once.

### 11. Files, nudged

The daemon emits `fs_changed { ws?: string }` (no `ws` = the bench's own tree) on
`tool_execution_end` of `write`, `edit`, `patch`, `bash`, `exec` in any session, debounced 500 ms
per target. A TUI whose files view shows that target bumps `filesRefresh`.

### 12. No ssh on the bench

The person: "Remove ssh completely." Every bench client already has a non-ssh door: the laptop
`kl-tui` over `kl-connect bench-proxy --tui` (wss to the daemon's TUI port), the browser over ttyd.
ssh to the bench goes:

- `kl-connect [team]` always runs `kl-tui --pipe <kl-connect> bench-proxy --tui [team]`. Gone:
  `Mode`, `modes`, `KL_DIRECT`, the fallback loop, `ssh_argv`, `--remote-tui`, the clipboard
  forward (`clip.rs`; the laptop TUI reads the laptop clipboard itself), `bench_known_hosts`. A
  missing `kl-tui` beside `kl-connect` is an error naming the path. Exit 3 (protocol mismatch) is
  reported, not retried another way.
- `kl-connect bench-proxy` loses its plain (sshd) mode; `--tui` stays as the only behaviour (the
  flag is kept so the pipe argv does not change).
- `kl-tui` loses `--ssh`; `--pipe` is the only transport.
- The bench image loses sshd: `bench/sshd_config`, `bench/sv/sshd/`, `bench/sv/kl-host/`,
  `bench/term/login-shell`, the `openssh-server` package and `BENCH_PORT` (pod port, gateway
  NetworkPolicy port, `Gateway.bench_port`). The gateway's `/tunnel/{ws}` refuses a bench ticket
  (401, as `/tui/` refuses a workspace ticket).
- **Claude login moves onto the wire.** `kl-connect claude login` ran `claude auth login` over
  `ssh -t`. It becomes a login option in the TUI's `/login` ("Claude (subscription)"): the daemon
  runs `claude auth login` under a pty (`script -qfec`), relays the sign-in URL as an `auth` notify
  event and the person's pasted code as the `auth.prompt` answer, exactly as pi's OAuth logins do.
  `kl-connect claude login` and its subcommand are deleted; `AUTH_MESSAGE` says "run /login in the
  TUI". A finished Claude login emits `auth_changed` (§9).

Not in this change (the person has not said): ssh into workspaces (`kl-connect ws ssh`, `ws ide`,
`ssh-config`, the gateway's workspace tunnel, `authorized_keys`).

## What does not change

- Keystrokes, drafts, focus, scroll, view, vim mode: per TUI (the person ruled out stroke sync).
- How TUIs connect beyond §12: direct `wss` from the laptop, ttyd through `relay.ts`.
- One agent per key, views over it, idle dispose with no views (`#settle`).
- Consent: what is typed through a client view counts as the person's words (`consent.ts`).

## Testing

Backend (`packages/backend`, `bun test` per package):

- Two `serve()` peers over in-memory wire pairs on one `LocalBackend` (pattern of
  `local.test.ts`), with a fake session factory:
  - A's `setModel` reaches B as `session_state`; B's later open does not change the model.
  - A's `/clear`: B gets `session_closed { reopen: true }`; B's reopen returns empty `messages`.
  - Two opens racing a rebuild produce one agent (count `createSession` calls).
  - A gated call emits `ask` to both; B answers; A gets `ask_resolved`; A's late answer is ignored.
  - Aborting the turn resolves the ask as reject and emits `ask_resolved`.
  - `hello` on a third connection returns the pending ask and the mode.
  - A prompt from A reaches B as `message_start` user with `shown` lacking the role card.
  - `space.watch`: a failing `space()` emits the last good view with `error`; an unchanged view
    emits nothing.
  - `forgetSessions` notifies watchers.

TUI (`apps/tui`, `bun test` per package; `sync.test.tsx` is the home):

- A pushed `session_state` sets the footer model; the TUI sends no model on reopen.
- `session_closed { reopen: true }` reopens and replaces entries.
- An `ask_resolved` drops the card.
- After "Allow always" for `bash` on key k, the next `ask` for `bash` on k is answered `once` with no
  card; an `ask` for `bash` on another key still shows one.
- A user `message_start` renders one row; a snapshot then the same live event renders one row.
- A space push with `error` keeps workspaces and focus.

Drill (laptop, two tmux TUIs on the user's bench, after shipping): for each of `/clear`, `/model`
across Claude/pi, codemode, prompt, permission card answered in the other TUI, abort with a card
open, workspace create and delete, `/login`: the other pane shows the result within one second
(space within 5 s), checked by `capture-pane`.

## Out of scope

- Server-side `workspace_wait`.
- Persisting always-allow across TUI restarts (a grant lives as long as the TUI process that made it).
- Sync between two benches (team bench vs personal): each daemon is its own world.
