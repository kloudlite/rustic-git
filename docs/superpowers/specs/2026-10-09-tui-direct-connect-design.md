# Laptop TUI: direct connection and live sync

Date: 2026-10-09. Status: approved in conversation, awaiting spec review.

## What the person asked for

- "I want all the connections open in sync. I want my TUI to connect and watch the sessions using
  WebSocket."
- "Can we remove ssh overhead and directly connect?"
- "We still need to have https."
- "Current ttyd should also work."

Success means three things:

1. The laptop `kl-tui` reaches the bench without ssh, over `wss://` (HTTPS) end to end.
2. Every client on one bench (laptop TUIs and the ttyd TUI in the pod) sees the same sessions,
   the same prompts and the same replies live, whoever typed them.
3. The ttyd TUI keeps working, unchanged in how it connects.

## Today

```
kl-connect ─spawn─► kl-tui ─spawn─► ssh ─ProxyCommand─► kl-connect bench-proxy
   bench-proxy ── wss://ws-<region>.khost.dev/tunnel/<bench> (Bearer bench-session token) ──►
   gateway (tunnel.rs: verify, resolve_bench, dial pod:7789, pump bytes) ──►
   sshd :7789 ── kl-host (relay.ts) ── ~/.kl/host.sock ── daemon.ts (one serve() per connection)

ttyd TUI in the pod (cli.tsx) ── relay.ts ── ~/.kl/host.sock   (same daemon)
```

Facts this design relies on (read in source, 2026-10-09):

- The gateway decides everything before the WebSocket upgrade, then pumps binary frames to a TCP
  socket (`bins/gateway/src/tunnel.rs`, `pump`). Ping and Pong are answered by axum. Any frame
  resets the 30-minute idle clock.
- The bench-session token is HS256, single-use (`jti` spent per replica), 60 s life,
  `typ: bench-session` (`crates/core/src/jwt.rs`). HS256 means the verifying key is also the
  signing key, so it can never be given to a bench pod.
- `allow_gateway_bench` (`crates/workspaces/src/k8s/bench.rs`) admits only the gateway pods to the
  bench pod's ports. The bench pods and the gateway run on k3s, which enforces NetworkPolicy.
  The ttyd route (`bins/gateway/src/term.rs`) already relies on exactly this: the gateway checks
  the token, ttyd itself checks nothing.
- The hostname's Cloudflare SSL mode is Flexible (`deploy/k3s/gateway.yaml` header). The edge to
  node hop is plain HTTP. Today ssh encryption covers that hop; without ssh, nothing would.
  Moving to Full (strict) needs an Origin CA certificate in a `gateway-tls` Secret and
  `GATEWAY_TLS_DIR`; the binary already serves 443 when it is set.
- The wire (`harness/packages/backend/src/wire.ts`) is JSON lines over a byte stream. It does not
  care how the bytes are chunked, so WebSocket message boundaries need no framing of their own.
- `connect(cmd)` (`harness/packages/backend/src/remote.ts`) spawns any command and speaks the wire
  over its stdin and stdout. It is not tied to ssh.
- The laptop TUI reads the laptop clipboard itself (`apps/tui/src/clipboard.ts`, osascript on
  macOS). The ssh `-R` clipboard forward (`KL_CLIP`) serves only the remote-TUI mode, where the
  TUI runs on the bench.
- `session.open` with no tools already attaches a plain view without reconfiguring the agent
  (`local.ts`, "An internal open ... carries none of the TUI's tools").

## Design

### 1. Transport: no ssh on the direct path

```
kl-connect ─spawn─► kl-tui --pipe kl-connect bench-proxy --tui [--team t]
   bench-proxy --tui ── wss://ws-<region>.khost.dev/tui/<bench> (Bearer bench-session token) ──►
   gateway (same checks, dial pod:BENCH_TUI_PORT, same pump) ──►
   daemon.ts TCP listener :BENCH_TUI_PORT (same serve() as host.sock)
```

- **Daemon.** `host()` listens on `0.0.0.0:7791` (`BENCH_TUI_PORT`) beside `host.sock`, with
  the same connection handler. Both carry the same JSON-lines wire. Port 7790 is taken by the
  workspace ttyd, so 7791.
- **Bench pod spec.** Add a `containerPort` named `tui` for 7791 and add 7791 to
  `allow_gateway_bench`. Nothing else may reach it.
- **Gateway.** New route `/tui/{bench}`. It accepts only a `bench-session` token, resolves with
  `resolve_bench(..., gw.tui_port)` and runs the same `reserve`/`spend`/`pump`. A workspace token
  on this route is 401. `Gateway::new` takes the extra port; `main.rs` passes
  `k8s::BENCH_TUI_PORT`.
- **kl-connect.** `bench-proxy --tui` mints the token exactly as `bench-proxy` does today and
  connects to `/tui/<bench>` instead of `/tunnel/<bench>`. It sends a WebSocket Ping every 15 s,
  because sshd's `ClientAliveInterval` no longer keeps the Cloudflare edge (100 s idle) awake. It
  exits 1 with "lost the bench connection" when no Pong arrives for 45 s, matching today's ssh
  3 × 15 s. A refused upgrade exits 1 with the gateway's status and reason on stderr.
- **kl-tui.** `remote.tsx` takes `--pipe <argv...>` beside `--ssh <argv...>`: `--pipe` runs the
  argv as given, `--ssh` prefixes `ssh`. `connect()` already handles both. Any failure before
  `hello` answers already makes `connect` throw code 3 (`remote.ts` `mismatch`), and stderr before
  boot is passed through, so a refused upgrade reaches kl-connect as exit 3 with its reason
  printed. No new exit-code plumbing.
- **Fallback order in `kl-connect bench`.** Direct first. On exit 3, laptop TUI over ssh (today's
  path). A 401 or 403 also falls back and fails the same way over ssh; that costs one extra token
  mint and is not worth a second exit code. On exit 3 again, remote TUI over `ssh -t` (today's fallback). The ssh paths stay as they
  are, so an old bench image keeps working.

**Auth.** The gateway token is the only lock on the direct path, with the NetworkPolicy keeping
everything but the gateway off port 7791. This is the same model the ttyd route uses today. The
daemon checks nothing (HS256 rules out giving it the key). Today's second lock, sshd wanting the
person's key, goes away on this path. That is the accepted cost of dropping ssh.

**HTTPS.** Prerequisite before the direct path is the default: the gateway hostnames move to
Cloudflare Full (strict), with an Origin CA certificate in `gateway-tls` and `GATEWAY_TLS_DIR` set.
That is a dashboard change and a Secret; the owner does the Cloudflare step. Until it lands,
`kl-connect bench` uses the direct path only when `KL_DIRECT=1` is set, so no prompt crosses the
edge to node hop in clear by default. Once Full (strict) is live, the default flips and
`KL_DIRECT=0` forces ssh.

**Not carried over:** the clipboard forward (the laptop TUI reads the laptop clipboard), the
`HostKeyAlias` pin (TLS to Cloudflare and the Origin CA certificate identify the far end), and the
`-t` pty (only the remote-TUI fallback needs it, and it keeps ssh).

### 2. ttyd

The ttyd TUI in the pod keeps `cli.tsx` → `relay.ts` → `host.sock`. It changes in no way except
that it receives the sync behaviour below, because that lives in the daemon. It is a required test
path: a laptop TUI and the ttyd TUI on one session at once.

### 3. Sync: every client sees the same thing

These four gaps exist on every transport. They are fixed in the daemon and the TUI, not in the
transport.

**Narrowed by the owner's ruling 2026-10-09:** no stroke-by-stroke transcript sync; only sessions
and their state. Items 1 and 3 below are out of scope and are not built; items 2 and 4 are.

1. **Prompts reach every view.** Today the TUI appends a typed prompt locally (`app.tsx:1233`) and
   its event handler drops non-assistant messages (`app.tsx:617`), so another client never sees
   what was typed. Change: the TUI stops appending its own prompt locally and renders user
   messages from the session's events, like assistant messages. One source of truth: the
   session's events.
2. **Session list is pushed.** Today every client polls the list and busy state every 5 s
   (`app.tsx:203`). Change: a new wire op `sessions.watch` makes the daemon emit
   `{ev: "sessions", key: "*", event: <list>}` whenever a session is created, removed, starts a
   turn or ends one, plus once right away. The TUI drops the 5 s poll when the bench answers the
   op. An old bench (op unknown) keeps the poll.
3. **Watching does not reconfigure.** Today `session.open` from a client with a different model or
   codemode switches or rebuilds the live agent under every other view (`local.ts` `session()`).
   Change: opening a session that already has a live agent adopts its current model, thinking
   level and codemode. `session.open` returns them and the TUI shows them. Only an explicit
   `setModel` / `setThinkingLevel` changes the agent, and that change is pushed to every view as
   an event. A tool missing from the live agent still rebuilds it, as today.
4. **The open session follows along.** A client already hears events for the session it opened.
   With the pushed list, a client sees every other session's busy state live and can switch to
   one and see its full history (`session.open` returns `messages`).

Out of scope (a later step, stated so nobody builds it now): tool calls and permission cards go
to the client whose tools the agent was built with, as today. Other views show the turn but do not
answer its cards.

## Errors

- Gateway refusals keep today's statuses, printed by kl-connect: 401 bad or spent token, 403
  paused member, 409 bench not ready, 404 no such route (old gateway), 502 dial failed (old bench
  image). Every one of them falls back to ssh through exit 3.
- Lost connection: kl-tui leaves with "lost the bench connection", as today. A running turn keeps
  going in the daemon. Reconnecting finds it busy, as today.
- A client that never calls `sessions.watch` behaves exactly as today.

## Testing

- **Gateway (cargo):** `/tui/{bench}` pumps to the TUI port (echo listener, as the `/tunnel`
  tests do); a workspace token on `/tui` is 401; a spent token is 401.
- **Bench pod spec (cargo):** the ports list asserts `[7789, 7681, 7791]`; the NetworkPolicy
  admits 7791 from the gateway only.
- **kl-connect (cargo):** `bench-proxy --tui` hits `/tui/<bench>` (mock `tunnel_handler`); a 404
  upgrade exits 1 naming the status; no Pong for 45 s exits 1. `bench()` runs the ssh path when the
  direct child exits 3.
- **Daemon (bun):** two clients, one on the unix socket and one on TCP, open one session.
  `sessions.watch` emits on turn start and end.
- **TUI (bun):** a pushed list renders without a fetch; the fetch stops when `sessions.watch`
  answers.
- **Live, on a bench restarted by the owner:** laptop direct and ttyd on one session, each sidebar shows
  the other's turn as busy; `KL_DIRECT=0` still connects over ssh; an old bench image still
  connects through the fallback.

## Rollout

1. Ship daemon, pod spec, gateway and kl-connect together. Direct is opt-in (`KL_DIRECT=1`).
2. Owner moves the gateway hostnames to Full (strict) with the Origin CA certificate.
3. Flip the default to direct.
4. ssh stays as the fallback until the direct path has run clean for a week. Removing it is a
   separate decision.
