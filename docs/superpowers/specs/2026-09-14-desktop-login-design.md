# Desktop app login — design

Status: draft for owner review · 2026-09-14

## Goal

The desktop app (harness/, Electron) opens to a login screen until the person is signed in to Kloudlite. After sign-in it finds the person's bench, wakes it, and connects — no `HARNESS_BENCH`, no hand-started `kl-connect bench`.

## Decisions (owner / defaults)

1. **Login first.** Until a valid credential exists, the only window content is the login screen. No cached sessions, no offline view, no pi child.
2. **Reuse the CLI device-code flow unchanged** (`POST /v1/cli/code` → browser `/cli/authorize?code=` → poll `GET /v1/cli/token`). No server change. Device label: `<hostname> (desktop)`.
3. **The token lives in the OS keychain**, via Electron `safeStorage` encrypting a file in `userData`, never plaintext and never in `~/.config/kl-connect/config.json`. The desktop and `kl-connect` logins are separate credentials, separately revocable.
4. **After sign-in the app connects itself** (option B). Team/region choice and laptop-session import are out of scope for this spec (later work).

## Flow

```
launch
  └─ credential in keychain?
       no  → Login screen
       yes → validate (GET /v1/cli/tokens or any cheap authed read)
               401 → clear credential → Login screen ("signed out: expired or revoked")
               ok  → Connect
Login screen
  [Sign in] → POST /v1/cli/code {device}
            → show code + open browser to /cli/authorize?code=…
            → poll /v1/cli/token every `poll` s, up to expiresIn (600 s)
                 202 pending → keep polling (show "waiting for approval", Cancel)
                 200 {token, expiresAt} → store → Connect
                 410 → "code expired or denied" → back to [Sign in]
Connect
  → pick bench (the person's own bench; team benches later)
  → POST /v1/bench/session
       202 waking → show "waking bench", retry with backoff
       409 stopped → start it (existing start route) then retry
       200 {id, token, gateway, expiresAt} → open tunnel → BenchClient → main window
```

## Components

| Unit | Where | Does |
|---|---|---|
| `auth/store.ts` | harness/src (main process) | load/save/clear the credential with `safeStorage`; `{api, token, expiresAt, username}`; refuses to run if `safeStorage.isEncryptionAvailable()` is false (shows an error instead of storing plaintext) |
| `auth/device.ts` | main | the device-code client: code, open browser (`shell.openExternal`, URL must match the configured API origin), poll, cancel |
| `connect/tunnel.ts` | main | the in-process equivalent of `kl-connect bench`: per TCP connection mint a single-use bench-session token and splice to the gateway `wss://…/tunnel/{id}`; exposes a 127.0.0.1 ephemeral port to `BenchClient` |
| IPC `auth:*`, `connect:*` | preload | `status`, `signIn`, `cancel`, `signOut`, `connectState` events; renderer never sees the token |
| `LoginScreen.tsx` | renderer | states: signed-out, waiting-for-approval (code shown, Cancel), expired/denied, error; `ConnectScreen` for waking/starting |
| Sign out | menu + settings | `DELETE /v1/cli/tokens/{jti}`, clear keychain file, close tunnel, back to login |

`HARNESS_BENCH` stays as a developer override: when set, skip Connect and use that URL (login is still required).

## Security

- Token only in the main process; renderer gets status, never the token (contextIsolation stays on).
- Browser open is restricted to `{api}/cli/authorize`; any other URL is refused.
- Credential file is `safeStorage`-encrypted, 0600, in `userData`; cleared on 401.
- No local HTTP listener for the redirect (device code needs none); the tunnel port binds 127.0.0.1 only.
- Revocation works as today: revoking the desktop login in the web's token list signs the app out on its next authed call.

## Errors and edge cases

- API unreachable at launch with a stored token: show "can't reach Kloudlite" with Retry; do not clear the credential.
- Token expires mid-use (30 days, no refresh): next 401 → sign-out → login screen with a reason.
- Bench stopped for quota/region reasons (409 with a message): show the message, no retry loop.
- Two app windows/instances: single-instance lock; the second focuses the first.

## Testing

- Unit (bench/test runner, node --test): device-code client against a stub server (202→200, 410, timeout, cancel); store round-trip with a fake `safeStorage`; 401 → clear.
- Tunnel: a stub gateway WebSocket; one token per TCP connection; token never reused.
- Renderer: login state machine transitions.
- SLO: a new probe row is not needed — `id.cli.flow` already covers the server flow.
- Manual acceptance: fresh profile → login screen → approve in browser → main window connected to the bench; sign out → login screen; revoke in web → app signs out.

## Out of scope

Importing laptop sessions, OAuth/passkey inside the app window, token refresh (server has none), Windows/Linux keychain differences beyond `safeStorage`.

## Team selection

Owner decision 2026-09-13: a bench belongs to a TEAM and the team owns the region, so the app offers no personal bench (that path is what failed with "choose a region for your personal bench").

- After sign-in the controller reads `GET /v1/bench/teams` (`[{slug, name, region}]`, the caller's current teams only; bearer identity with CLI revocation check, uncached membership, 503 when the directory is unreadable, `Cache-Control: no-store`, per-IP limit `KLOUDLITE_BENCH_TEAMS_LIMIT`, default 60/60). It sits under the existing `/v1/bench` ingress path.
- New phase `choose-team`. A lone team with a region is picked automatically. A team with `region: ""` is shown disabled ("no region yet — ask an admin") and can't be chosen, so no bench is created for it.
- The choice is saved in `userData/team.txt` (a plain setting). On the next launch it is checked against the live list. A team the person left, or one without a region, clears the setting and shows the picker with the reason.
- Connect passes the team on `POST /v1/bench` (body) and on `/v1/bench/session` and `/v1/bench/start` (`?team=`).
- The Account page shows the team with "Switch team" (disconnect, forget, back to the picker). The picker, connecting and error screens all offer "Sign out".

## Open questions

1. Which API base does the app use by default (prod `kloudlite.io` vs a settings field)? Default: a build-time constant with a settings override.
2. Should the desktop login appear in the web token list as a distinct kind ("desktop") rather than a CLI login? Default: same kind, distinguished by device label.
