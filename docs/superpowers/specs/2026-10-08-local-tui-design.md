# Local TUI over a remote bench

Date: 2026-10-08. Status: approved direction, spec for review.

## Problem

The harness TUI runs inside the bench pod; the laptop sees it through `kl-connect` → ssh over the
gateway WebSocket → sshd (7789), or the browser through ttyd. Every keystroke and every redraw makes
that round trip, so typing and scrolling lag badly.

## Decision

Render on the laptop; keep everything else in the pod.

- The TUI talks to ONE interface, `Backend`, for every touchpoint that is not a terminal concern.
- Two implementations: `LocalBackend` (direct calls into `@kloudlite-tui/agent`, `git`, the
  filesystem; exactly today's behaviour) and `RemoteBackend` (the same calls as JSON lines over an
  ssh channel).
- In the pod, a host process serves `LocalBackend` over stdio. Laptop TUI = `RemoteBackend` ↔ ssh
  ↔ host ↔ `LocalBackend`.
- The in-pod TUI (sshd ForceCommand and ttyd) stays and uses `LocalBackend`. One rendering code
  path for both, so they cannot drift.
- Nothing is stored on the laptop: sessions, transcripts, settings, auth, model cache all stay in
  the pod's home volume. The laptop binary holds no state between runs.

## Inventory (what goes behind `Backend`)

From a full scan of `harness/apps/tui/src` (71 touchpoints):

- **A. Session control and events.** `createSession`, `subscribe` (all event kinds: agent, message,
  tool_execution, queue, compaction, auto_retry), `prompt`/`steer`/`followUp` (with images),
  `clearQueue`, `abort`, `dispose`, `setModel`, `setThinkingLevel`, `setAutoCompactionEnabled`,
  `.messages` on restore, `restored`, `isClaude`, `resolveModel`.
- **A'. Callbacks from the agent into the UI.** The permission gate (`agent.beforeToolCall`,
  `app.tsx:861`) and TUI-owned registry tools (`question`, the `envApi` tools, `app.tsx:726-812`).
  These are calls the pod makes to the laptop and must wait for an answer.
- **B. Session store.** `listSessions`, `nameSession`, `describeSession`, `clearSessionHistory`.
- **C. Settings, auth, models.** `readSettings`/`writeSettings` (8 sites), `listModels`,
  `models.refresh`, `getModel`, `providerAuth` (reads provider env vars: must be the POD's env),
  `loginOptions`, `loginProvider` (with `auth_url`/prompt events), `claudeSignedIn`.
- **D. Workspace filesystem and git.** `git.ts` (`isGitRepo`, `changes`, `fileDiff`, `fullFile`,
  `listDir`, `grep`, `displayRoot`), `diff.ts` `toolDiff` (reads the edited file), `process.cwd()`.
- **Stays local (F).** Keyboard, mouse, terminal size, renderer, modifyOtherKeys escapes,
  clipboard image read (bytes go out with the prompt), opening the OAuth URL in the browser,
  `KLOUDLITE_THEME`.

## Interface

`harness/packages/backend` (new), `src/index.ts` exports:

```ts
interface Backend {
  hello(): Promise<Hello>; // settings, model catalog, providerAuth, loginOptions,
                           // cwd, home, registry names, protocol version
  session(key: string, opts: SessionOpts): Promise<SessionHandle>;
  sessions: { list(base?): Promise<Meta[]>; name(k, n); describe(k, d); clear(k) };
  settings: { write(patch): Promise<void> };
  models: { refresh(): Promise<Model[]> };
  auth: { login(provider, mode, ui: LoginUi, signal); claudeSignedIn(): Promise<boolean> };
  fs: { isGitRepo; changes; fileDiff; fullFile; listDir; grep; toolDiff }; // all async
  onCall(h: { permission(req): Promise<Decision>; tool(name, args): Promise<Result> }): void;
}
interface SessionHandle { // the 10 control calls + subscribe + snapshot fields
  messages; restored; isClaude; model;
  prompt; steer; followUp; clearQueue; abort; dispose;
  setModel; setThinkingLevel; setAutoCompactionEnabled;
  subscribe(cb): () => void;
}
```

Rules:
- Every `D` call becomes async. TUI call sites move to effects/state; this is the largest part of
  the TUI change.
- `hello()` replaces the import-time `readSettings()`/`listModels()` reads in `app.tsx`,
  `models.ts`, `theme.ts`: the TUI renders after `hello` resolves.
- The permission gate moves into the backend: `LocalBackend` installs `beforeToolCall` and calls
  the registered `permission` handler, with the `toolDiff` preview computed beside the files.
- TUI-owned tools are registered by name in `SessionOpts`; their `run` is forwarded to
  `onCall().tool`. Host-side tools (`webFetch`, `webSearch`) are added by the host.

## Wire protocol

JSON lines over the ssh channel's stdio, one object per line.

- Client → host: `{id, op, args}`; host → client: `{id, ok, value}` or `{id, ok:false, error}`.
- Host → client events: `{ev, key, event}` (session events tagged by session key, so one channel
  multiplexes every session the TUI holds).
- Host → client calls (permission, TUI tools, login prompts): `{cid, call, args}`; client answers
  `{cid, ok, value}`. The host awaits the answer; abort cancels it.
- First exchange is `hello` carrying `protocol: 1`. Mismatch: the laptop binary exits with a clear
  message and `kl-connect` falls back to the in-pod TUI.
- Images travel base64 inside the prompt args (they already are base64 in pi messages).
- Disconnect = host exits = sessions disposed, the same as closing ssh today.

## Transport and launch

- No new port, ingress or auth: the existing `kl-connect` ProxyCommand → gateway → sshd path.
- `bench/term/login-shell` dispatches on `SSH_ORIGINAL_COMMAND`: `kl-host` runs
  `bun run /opt/kl/harness/packages/backend/src/serve.ts`; anything else keeps running the TUI;
  `claude-login` unchanged.
- `bench/sessions/main.ts` idle tracking counts host processes as well as TUI processes, or a bench
  idle-stops under a live laptop session.
- `kl-connect [team]`: if `kl-tui` sits next to the `kl-connect` binary (or on PATH), run
  `kl-tui --ssh <the ssh argv kl-connect builds today> kl-host`; else, or with `--remote-tui`, the
  current `ssh -t` path.

## Packaging

- `bun build --compile` of `harness/apps/tui/src/cli.tsx` into `kl-tui` for darwin-arm64,
  darwin-x64 and linux-x64, built by `.github/workflows/kl-connect.yml` and shipped beside
  `kl-connect`.
- First plan task is a spike: does `@opentui`'s native library (bun:ffi) survive `--compile`? If
  not, the binary carries the library beside it.
- Bench image (Dagger `imageBench`, `deploy/bench/Dockerfile`) needs no new runtime: the host is
  bun from the existing `/opt/kl/harness` install.

## Known limits

- pi OAuth flows that use a localhost callback server in the pod cannot complete from the laptop
  browser. Same as today's remote TUI; the paste-code path still works.
- Laptop and pod must agree on the protocol version; old laptop binaries fall back to the in-pod
  TUI rather than half-working.
- A session does not outlive the ssh connection (unchanged). Keeping the host alive across
  reconnects is a later step.

## Testing

- Protocol round trip in-process: `RemoteBackend` ↔ pipe ↔ `serve` over a `LocalBackend` with a
  fake session; cover events, permission call, TUI tool call, abort while a call is pending,
  version mismatch, disconnect.
- Existing TUI tests (`app.test.tsx` etc.) run against `LocalBackend`.
- Live: `kl-tui` on the laptop against a bench; type and scroll, Claude and pi turns, permission
  card, session list, git panel, image paste.
