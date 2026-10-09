# TUI Direct Connect Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The laptop kl-tui reaches the bench daemon over wss with no ssh in the path, and every
client (laptop TUIs, the ttyd TUI) sees the same sessions and their busy state live.

**Architecture:** The gateway grows a `/tui/{bench}` route that pumps a bench-session WebSocket to
a new daemon TCP listener on port 7791, which runs the same `serve` handler as `host.sock`.
kl-connect's `bench-proxy --tui` is the pipe kl-tui spawns (`--pipe`), with WebSocket keepalive;
`kl-connect bench` tries direct, then today's two ssh paths, each on exit 3. Sync is fixed in the
daemon and the TUI, not the transport: the session list and busy state are pushed
(`sessions.watch`). No stroke-by-stroke transcript sync (owner ruling 2026-10-09).

**Tech Stack:** Rust (axum, tokio-tungstenite, k8s-openapi), Bun + TypeScript (node:net, opentui
React), the `./wire` line protocol (`Peer`).

**Spec:** `docs/superpowers/specs/2026-10-09-tui-direct-connect-design.md`

## Global Constraints

- Daemon TUI port is `7791` (`BENCH_TUI_PORT`; 7790 is the workspace ttyd). containerPort name `tui`.
- `/tui/{bench}` accepts a `bench-session` token only; a workspace token is 401.
- `allow_gateway_bench` admits 7791 from the gateway only. Nothing else may reach it.
- `bench-proxy --tui` pings every 15 s; no frame for 45 s exits 1 with `lost the bench connection`.
- A refused upgrade exits 1 with the HTTP status on stderr.
- Fallback order: direct, then laptop ssh, then remote `ssh -t`, each step taken on exit 3.
- Direct runs only with `KL_DIRECT=1` until Cloudflare Full (strict) + Origin CA `gateway-tls` +
  `GATEWAY_TLS_DIR` are live (the owner's step). This plan does NOT flip the default.
- Direct path carries no clipboard forward, no `HostKeyAlias`, no `-t`.
- "A client that never calls `sessions.watch` behaves exactly as today."
- ttyd (`cli.tsx` → `relay.ts` → `host.sock`) is unchanged code; it gains sync through the daemon.
- Out of scope: answering tool calls or permission cards from a client other than the one the
  agent was built with.
- Commits: plain imperative sentence-case subjects, no tool or model names, no trailers.
- Checks. Cargo, from `/Volumes/kdisk/rustic-git-wt/master`, with
  `CARGO_TARGET_DIR=/Volumes/kdisk/target-master`: `cargo test -p <crate>` and
  `cargo clippy --workspace --all-targets -- -D warnings`. Harness: `cd harness && bun run check`,
  then `bun test` inside each of `packages/backend`, `packages/agent`, `packages/tools`,
  `apps/tui` (never at the harness root).

## Spec corrections (rulings made while planning; the spec text is superseded where it differs)

1. **The 5 s poll is not the session list.** `app.tsx:201-205` polls `backend().space()`
   (workspaces, environments, tasks) and STAYS. The session list is fetched only at
   `app.tsx:281-284`, on `activeBase`/name/describe changes. `sessions.watch` replaces that fetch
   and also supplies busy for sessions this client has not opened. An old bench keeps the fetch.
2. **Sync is sessions and their state only** (owner ruling 2026-10-09). Spec section 3 items 1
   and 3 (prompts on every view; watching does not reconfigure) are out of scope. Only item 2
   (pushed session list with busy) and item 4 (follow along through the list) are built.

## Review Focus

1. **A turn started in one view.** Expect the other view's sidebar to mark that session busy
   without opening it — pinned in Task 6 (`sessions.watch` emits on turn start and end; the sync
   app test renders a pushed busy list).
2. **An old bench without `sessions.watch`.** Expect today's list fetch — pinned in Task 6
   (RemoteBackend rejects, TUI falls back, `list` called).
3. **Cloudflare drops idle sockets at 100 s.** Expect a quiet session to stay connected — pinned in
   Task 4 (keepalive sends Pings at the interval; a peer that answers keeps the pump alive past 3×).

---

### Task 1: Bench pod exposes the TUI port to the gateway only

**Files:**
- Modify: `crates/workspaces/src/k8s/bench.rs:25-27` (const), `:107-110` (ports), `:184-187` (netpol)
- Test: `crates/workspaces/src/k8s/tests/bench.rs:67` and `:177`

**Interfaces:**
- Produces: `pub const BENCH_TUI_PORT: u16 = 7791;` in `kloudlite_workspaces::k8s` (re-exported
  the same way as `BENCH_TERM_PORT`; check `crates/workspaces/src/k8s/mod.rs` and add it to the
  same `pub use` line if constants are listed there).

- [ ] **Step 1: Update both tests to expect the new port**

In `crates/workspaces/src/k8s/tests/bench.rs`, the container-ports assertion right after line 67
becomes:

```rust
    assert_eq!(ports, [BENCH_PORT as i32, BENCH_TERM_PORT as i32, BENCH_TUI_PORT as i32]);
```

and the NetworkPolicy assertion at line 177 becomes:

```rust
    assert_eq!(ports, [BENCH_PORT as i32, BENCH_TERM_PORT as i32, BENCH_TUI_PORT as i32]);
```

Add `BENCH_TUI_PORT` to that file's `use` of the bench constants.

- [ ] **Step 2: Run, expect a compile failure**

Run: `CARGO_TARGET_DIR=/Volumes/kdisk/target-master cargo test -p kloudlite-workspaces k8s::tests::bench 2>&1 | tail -5`
Expected: FAIL, `cannot find value BENCH_TUI_PORT`.

- [ ] **Step 3: Implement**

After line 27 of `crates/workspaces/src/k8s/bench.rs`:

```rust
/// The bench daemon's TUI listener (`harness/packages/backend/src/daemon.ts` `listenTcp`): the
/// laptop kl-tui reaches it through the gateway's `/tui/{bench}` route with no ssh in the path.
/// 7790 is the workspace ttyd, so the daemon takes the next one. The gateway token is its only
/// lock; `allow_gateway_bench` is what keeps every other pod off it.
pub const BENCH_TUI_PORT: u16 = 7791;
```

Container ports:

```rust
        ports: Some(vec![
            ContainerPort { container_port: BENCH_PORT as i32, name: Some("ssh".into()), ..Default::default() },
            ContainerPort { container_port: BENCH_TERM_PORT as i32, name: Some("ttyd".into()), ..Default::default() },
            ContainerPort { container_port: BENCH_TUI_PORT as i32, name: Some("tui".into()), ..Default::default() },
        ]),
```

NetworkPolicy ports:

```rust
                    "ports": [
                        { "protocol": "TCP", "port": BENCH_PORT as i32 },
                        { "protocol": "TCP", "port": BENCH_TERM_PORT as i32 },
                        { "protocol": "TCP", "port": BENCH_TUI_PORT as i32 },
                    ],
```

Update the `//!` header line 13 to name the daemon's TUI port (`BENCH_TUI_PORT`) beside sshd and ttyd.

- [ ] **Step 4: Run, expect pass**

Run: `CARGO_TARGET_DIR=/Volumes/kdisk/target-master cargo test -p kloudlite-workspaces k8s::tests::bench 2>&1 | tail -3`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/k8s
git commit -m "Expose the bench daemon TUI port to the gateway"
```

---

### Task 2: Gateway `/tui/{bench}` route

**Files:**
- Modify: `bins/gateway/src/tunnel.rs:36-80` (struct, `new`), `:192-204` (`app`), `:206-275` (handler)
- Modify: `bins/gateway/src/main.rs:57-64`; `bins/gateway/src/tunnel.rs:367` (test caller);
  `bins/gateway/tests/term.rs:96`
- Create: `bins/gateway/tests/tui.rs`

**Interfaces:**
- Consumes: `kloudlite_workspaces::k8s::BENCH_TUI_PORT` (Task 1).
- Produces: `Gateway::new(jwt, region, kube, ssh_port, bench_port, term_port, tui_port)`;
  route `GET /tui/{bench}`.

- [ ] **Step 1: Write the failing test**

Create `bins/gateway/tests/tui.rs`. Copy `bins/gateway/tests/term.rs`'s top half verbatim (its
`SECRET`, `REGION`, `BENCH`, `BENCH_POD` consts, the `bench()` and `pod(ip)` JSON fixtures, and the
`kube_test::{get, mock_client, Route}` routes it builds) so the bench resolves to `127.0.0.1`.
Then:

```rust
//! `/tui/{bench}`: the laptop kl-tui's path to the bench daemon's TCP listener, no ssh. Same
//! token, reserve, spend and pump as `/tunnel`; only a bench-session token is accepted.
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

async fn echo() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    port
}

/// Serves the gateway with the TUI port at `tui` and every other port at a closed one, so a
/// pump to the wrong port is a 502, not a silent pass.
async fn serve(tui: u16) -> std::net::SocketAddr {
    let gw = std::sync::Arc::new(kloudlite_gateway::tunnel::Gateway::new(
        jwt(),
        REGION.into(),
        client(),
        1,
        1,
        1,
        tui,
    ));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, kloudlite_gateway::tunnel::app(gw)).await.unwrap() });
    addr
}

fn req(addr: std::net::SocketAddr, token: &str) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut r = format!("ws://{addr}/tui/{BENCH}").into_client_request().unwrap();
    r.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    r
}

#[tokio::test]
async fn tui_route_pumps_to_the_tui_port() {
    let addr = serve(echo().await).await;
    let (token, _) = jwt().mint_bench_session("karthik", BENCH, REGION).unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(req(addr, &token)).await.unwrap();
    ws.send(Message::Binary(b"hello\n".to_vec().into())).await.unwrap();
    let Some(Ok(Message::Binary(b))) = ws.next().await else { panic!("no echo") };
    assert_eq!(&b[..], b"hello\n");
}

#[tokio::test]
async fn a_workspace_token_is_refused_on_the_tui_route() {
    let addr = serve(echo().await).await;
    let (token, _) = jwt().mint_ssh_session("karthik", BENCH, REGION).unwrap();
    let err = tokio_tungstenite::connect_async(req(addr, &token)).await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
}

#[tokio::test]
async fn a_spent_token_is_refused_on_the_tui_route() {
    let addr = serve(echo().await).await;
    let (token, _) = jwt().mint_bench_session("karthik", BENCH, REGION).unwrap();
    let first = tokio_tungstenite::connect_async(req(addr, &token)).await;
    assert!(first.is_ok());
    let err = tokio_tungstenite::connect_async(req(addr, &token)).await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
}
```

`jwt()` and `client()` are the helpers `term.rs` already uses to build the `Jwt` from `SECRET` and
the mock kube client; copy them with the fixtures (rename only if `term.rs` names them otherwise,
and use those names here). The owner string must match the fixture bench's owner; use the value
`term.rs` mints with.

- [ ] **Step 2: Run, expect failure**

Run: `CARGO_TARGET_DIR=/Volumes/kdisk/target-master cargo test -p kloudlite-gateway --test tui 2>&1 | tail -5`
Expected: FAIL to compile (`Gateway::new` takes 6 arguments).

- [ ] **Step 3: Implement**

In `Gateway`, after `term_port`:

```rust
    /// `BENCH_TUI_PORT` everywhere real; a test points it at a local echo listener.
    pub tui_port: u16,
```

`new` gains `tui_port: u16` as its last parameter and sets the field. In `app()`:

```rust
        .route("/tunnel/{ws}", get(tunnel))
        .route("/tui/{bench}", get(tui_tunnel))
```

Replace `async fn tunnel(...)` with a thin pair over one body:

```rust
async fn tunnel(
    State(gw): State<Arc<Gateway>>,
    Path(ws): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    connect(gw, ws, headers, upgrade, false).await
}

/// The laptop kl-tui's direct path: the bench daemon's TCP listener instead of sshd. Bench tokens
/// only — a workspace has no daemon, and a workspace token that reached a bench's port would be a
/// token for one object opening another.
async fn tui_tunnel(
    State(gw): State<Arc<Gateway>>,
    Path(bench): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    connect(gw, bench, headers, upgrade, true).await
}

async fn connect(gw: Arc<Gateway>, ws: String, headers: HeaderMap, upgrade: WebSocketUpgrade, tui: bool) -> Response {
```

The body is today's `tunnel` body unchanged except two lines. After the `ticket` match:

```rust
    if tui && matches!(ticket, Ticket::Workspace(_)) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
```

and the bench resolve arm:

```rust
        Ticket::Bench(_) => resolve_bench(&gw.kube, &ws, if tui { gw.tui_port } else { gw.bench_port }).await,
```

Callers: `main.rs` passes `kloudlite_workspaces::k8s::BENCH_TUI_PORT` as the new last argument;
`tunnel.rs:367` test passes `client, 22, 7789, 7681, 7791`; `tests/term.rs:96` passes
`client, 22, 7789, term_port, 7791`.

The comment `// ssh is interactive: …` above `set_nodelay` becomes `// ssh and the TUI wire are
interactive: …`.

- [ ] **Step 4: Run, expect pass**

Run: `CARGO_TARGET_DIR=/Volumes/kdisk/target-master cargo test -p kloudlite-gateway 2>&1 | grep -E "^test result|FAILED|panicked" | head`
Expected: every `test result: ok`.

- [ ] **Step 5: Commit**

```bash
git add bins/gateway
git commit -m "Route /tui/{bench} to the bench daemon TUI port"
```

---

### Task 3: Daemon listens on TCP beside host.sock

**Files:**
- Modify: `harness/packages/backend/src/daemon.ts:1-60`
- Test: `harness/packages/backend/src/daemon.test.ts`

**Interfaces:**
- Produces: `export function attach(backend: Backend): (conn: Socket) => void`;
  `export function listenTcp(backend: Backend, port: number, hostname = "0.0.0.0"): Promise<Server>`.
  `host(backend, sock)` keeps its signature.

- [ ] **Step 1: Write the failing test**

Append to `daemon.test.ts` (it already imports `connect as netConnect` from `node:net`, `Peer`,
`fakeBackend`):

```ts
import { listenTcp } from "./daemon.ts";
import { RemoteBackend } from "./remote.ts";

/** A client on the daemon's TCP port, the way the gateway pump reaches it. */
export function tcpClient(port: number) {
  const conn = netConnect({ host: "127.0.0.1", port });
  const peer = new Peer((line) => void conn.write(line));
  conn.on("data", (c: Buffer) => peer.feed(c));
  return { backend: new RemoteBackend(peer), close: () => conn.destroy() };
}

test("the TCP listener speaks the same wire as host.sock", async () => {
  const server = await listenTcp(fakeBackend().b, 0, "127.0.0.1");
  const port = (server.address() as any).port;
  const c = tcpClient(port);
  expect((await c.backend.hello()).protocol).toBe(1);
  c.close();
  server.close();
});

test("a TCP client leaving disposes its views", async () => {
  const { b, disposed } = fakeBackend();
  const server = await listenTcp(b, 0, "127.0.0.1");
  const c = tcpClient((server.address() as any).port);
  await c.backend.session("main", { model: { provider: "x", id: "y" }, tools: [] });
  c.close();
  await new Promise((r) => setTimeout(r, 50));
  expect(disposed).toEqual(["main"]);
  server.close();
});
```

If `Peer.feed` takes a string in `wire.ts`, pass `c.toString()`; match what `host` does today.

- [ ] **Step 2: Run, expect failure**

Run: `cd harness/packages/backend && bun test src/daemon.test.ts 2>&1 | tail -5`
Expected: FAIL, `listenTcp` is not exported.

- [ ] **Step 3: Implement**

```ts
/** One client connection, whatever it arrived on: one `serve`, its views disposed when it closes. */
export function attach(backend: Backend) {
  return (conn: Socket) => {
    const peer = new Peer((line) => void conn.write(line));
    const s = serve(backend, peer);
    conn.setNoDelay?.(true);
    conn.on("data", (c: Buffer) => peer.feed(c));
    conn.on("close", () => {
      peer.close();
      void s.dispose();
    });
    conn.on("error", () => {});
  };
}

export function host(backend: Backend, sock: string): Promise<Server> {
  rmSync(sock, { force: true }); // a crashed daemon leaves its socket file behind
  const server = createServer(attach(backend));
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(sock, () => {
      chmodSync(sock, 0o600);
      resolve(server);
    });
  });
}

/** The laptop kl-tui's door: the gateway's `/tui/{bench}` pumps here. No auth of its own — the
 * gateway token is the lock and the pod's NetworkPolicy admits only the gateway to this port
 * (`crates/workspaces/src/k8s/bench.rs` `BENCH_TUI_PORT`). */
export function listenTcp(backend: Backend, port: number, hostname = "0.0.0.0"): Promise<Server> {
  const server = createServer(attach(backend));
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, hostname, () => resolve(server));
  });
}
```

Import `type Socket` from `node:net`. In the `import.meta.main` block, after `await host(b, sockPath());`:

```ts
  // 7791 = BENCH_TUI_PORT (crates/workspaces/src/k8s/bench.rs); env only so a laptop run can move it
  await listenTcp(b, Number(process.env.KL_HOST_TUI_PORT ?? 7791));
```

Rewrite the `//!` header's second and eighth lines: clients reach the daemon on the unix socket
(ttyd and the ssh relay) or on TCP 7791 (the laptop kl-tui through the gateway); the socket is 0600,
and the TCP port is fenced by the pod's NetworkPolicy to the gateway, whose token is the lock.

- [ ] **Step 4: Run, expect pass**

Run: `cd harness/packages/backend && bun test src/daemon.test.ts 2>&1 | tail -3`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add harness/packages/backend/src/daemon.ts harness/packages/backend/src/daemon.test.ts
git commit -m "Serve the TUI wire on TCP 7791 beside host.sock"
```

---

### Task 4: kl-connect `bench-proxy --tui`, keepalive, direct-first fallback

**Files:**
- Modify: `bins/kl-connect/src/proxy.rs:84-128` (`pump_io`), its callers
- Modify: `bins/kl-connect/src/bench.rs:18-67` (`bench`), `:103-140` (`proxy`, `serve`), tests
- Modify: `bins/kl-connect/src/main.rs:73`, `:182`

**Interfaces:**
- Consumes: gateway route `/tui/{bench}` (Task 2).
- Produces: `pub async fn proxy(team: Option<&str>, tui: bool)`;
  `pump_io(ws, r, w, keepalive: Option<Duration>)`; `fn modes(direct: bool, laptop: bool) -> Vec<Mode>`;
  CLI `kl-connect bench-proxy --tui [team]`.

- [ ] **Step 1: Write the failing tests**

In `bins/kl-connect/src/bench.rs`'s test module (next to `each_connection_gets_its_own_tunnel_and_token`,
reusing its `ENV_LOCK`, `KL_CONFIG_DIR`, `KL_GATEWAY_OVERRIDE` setup and `session_handler_ok`):

```rust
    #[test]
    fn modes_try_direct_only_when_asked_and_the_laptop_tui_exists() {
        use Mode::*;
        assert_eq!(modes(true, true), [Direct, LaptopSsh, RemoteSsh]);
        assert_eq!(modes(false, true), [LaptopSsh, RemoteSsh]);
        assert_eq!(modes(true, false), [RemoteSsh]);
        assert_eq!(modes(false, false), [RemoteSsh]);
    }

    #[tokio::test]
    async fn tui_proxy_dials_the_tui_route() {
        // same mock api + gateway as each_connection_gets_its_own_tunnel_and_token, but the
        // gateway router records the path it was hit on
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let h = hits.clone();
        let gw = axum::Router::new().route(
            "/{kind}/{bench}",
            axum::routing::get(move |axum::extract::Path((kind, _b)): axum::extract::Path<(String, String)>, up: axum::extract::WebSocketUpgrade| {
                h.lock().unwrap().push(kind);
                async move { up.on_upgrade(|mut s| async move { let _ = s.close().await; }) }
            }),
        );
        let (cfg, _guard) = connection(gw).await; // existing helper: api + gateway + env, returns Config
        let (_a, b) = tokio::io::duplex(64);
        let (r, w) = tokio::io::split(b);
        serve(&cfg, None, r, w, true).await.unwrap();
        assert_eq!(*hits.lock().unwrap(), ["tui"]);
    }

    #[tokio::test]
    async fn a_refused_upgrade_names_the_status() {
        let gw = axum::Router::new(); // every path 404s
        let (cfg, _guard) = connection(gw).await;
        let (_a, b) = tokio::io::duplex(64);
        let (r, w) = tokio::io::split(b);
        let err = serve(&cfg, None, r, w, true).await.unwrap_err();
        assert!(err.contains("404"), "{err}");
    }
```

If `connection(cfg)` today takes something other than a gateway router, extend it to take the
gateway `Router` (the existing callers pass their current `tunnel_handler` router) and keep its
return shape; do not write a second harness.

In `bins/kl-connect/src/proxy.rs` tests (add a `#[cfg(test)] mod tests` if none):

```rust
    /// A gateway that upgrades and then never sends a frame or answers a Ping.
    async fn silent() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(s).await.unwrap();
            // hold the socket open, read nothing: tungstenite only answers Pings while read
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            drop(ws);
        });
        format!("ws://{addr}/tui/b")
    }

    /// A gateway that reads (so tungstenite answers Pings) but sends no data.
    async fn quiet() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
            while let Some(Ok(_)) = futures::StreamExt::next(&mut ws).await {}
        });
        format!("ws://{addr}/tui/b")
    }

    #[tokio::test]
    async fn no_pong_ends_the_pump_with_lost_connection() {
        let ws = connect(&silent().await, "t").await.unwrap();
        let (_a, b) = tokio::io::duplex(64);
        let (r, w) = tokio::io::split(b);
        let ka = std::time::Duration::from_millis(50);
        let err = tokio::time::timeout(std::time::Duration::from_secs(2), pump_io(ws, r, w, Some(ka)))
            .await
            .expect("pump must give up on its own")
            .unwrap_err();
        assert_eq!(err, "lost the bench connection");
    }

    #[tokio::test]
    async fn pongs_keep_a_quiet_connection_alive() {
        let ws = connect(&quiet().await, "t").await.unwrap();
        let (_a, b) = tokio::io::duplex(64);
        let (r, w) = tokio::io::split(b);
        let ka = std::time::Duration::from_millis(50);
        // 10 × the keepalive with no data: well past the 3× limit, so only Pongs keep it up
        let res = tokio::time::timeout(std::time::Duration::from_millis(500), pump_io(ws, r, w, Some(ka))).await;
        assert!(res.is_err(), "pump ended early: {res:?}");
    }
```

- [ ] **Step 2: Run, expect failure**

Run: `CARGO_TARGET_DIR=/Volumes/kdisk/target-master cargo test -p kl-connect 2>&1 | tail -5`
Expected: FAIL to compile (`modes`, `Mode`, `serve` arity, `pump_io` arity).

- [ ] **Step 3: Implement `pump_io` keepalive**

Replace `pump_io` in `proxy.rs`:

```rust
/// Pumps binary frames between an open tunnel and any reader/writer pair — stdio for `ws proxy`
/// and `bench-proxy`, an in-memory pipe in tests. Writes are flushed per frame (a request/response
/// handshake on the other end), and a `Close` frame ends the pump cleanly while any other error is
/// reported.
///
/// `keepalive`: `bench-proxy --tui` has no sshd under it, whose `ClientAliveInterval` kept the
/// Cloudflare edge (100 s idle) awake, so it Pings every interval and gives up after three with no
/// frame of any kind back — the same 3 × 15 s an ssh session allows today.
pub(crate) async fn pump_io<
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
>(
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    mut r: R,
    mut w: W,
    keepalive: Option<Duration>,
) -> Result<(), String> {
    let (mut tx, mut rx) = ws.split();
    // The reader lives in its own task: writer and reader progress independently, and a read
    // that blocks would deadlock the write half. The Pings go out from here too, so the write
    // half has one owner.
    let up = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        let mut tick = keepalive.map(tokio::time::interval);
        loop {
            let ping = async {
                match tick.as_mut() {
                    Some(t) => t.tick().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                n = r.read(&mut buf) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                },
                _ = ping => {
                    if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx.close().await;
    });

    let limit = keepalive.map(|k| k * 3);
    loop {
        let next = match limit {
            Some(l) => match tokio::time::timeout(l, rx.next()).await {
                Ok(m) => m,
                Err(_) => {
                    up.abort();
                    return Err("lost the bench connection".to_string());
                }
            },
            None => rx.next().await,
        };
        let Some(msg) = next else { break };
        match msg {
            Ok(Message::Binary(b)) => {
                w.write_all(&b).await.map_err(|e| e.to_string())?;
                w.flush().await.map_err(|e| e.to_string())?;
            }
            Ok(Message::Close(_)) => break,
            // A dropped tunnel is not a clean end of session: the caller must see a failure, and
            // the error kind (never the token, which appears in no frame) is the one line printed.
            Err(e) => {
                up.abort();
                return Err(format!("tunnel error: {e}"));
            }
            Ok(_) => {} // Pong and anything else: already counted as a sign of life by the timeout
        }
    }
    up.abort();
    Ok(())
}
```

`use std::time::Duration;` at the top of `proxy.rs` if absent. Every existing caller of `pump_io`
(grep `pump_io(` in `bins/kl-connect/src`) passes `None` as the new last argument.

`connect`'s error already reads `gateway unreachable: HTTP error: 404 Not Found` for a refused
upgrade (tungstenite's `Error::Http` displays the status); `a_refused_upgrade_names_the_status`
holds that. Do not change `connect`.

- [ ] **Step 4: Implement `serve(.., tui)` and `proxy(team, tui)`**

In `bench.rs`:

```rust
/// `kl-connect bench-proxy [--tui] [team]`. Plain: ssh's ProxyCommand, stdio pumped to the bench's
/// sshd. `--tui`: the pipe the laptop kl-tui runs (`kl-tui --pipe …`), stdio pumped to the bench
/// daemon's TUI port through the gateway's `/tui/{bench}` — no ssh anywhere.
pub async fn proxy(team: Option<&str>, tui: bool) -> Result<(), String> {
    let cfg = crate::config::load()?;
    serve(&cfg, team, tokio::io::stdin(), tokio::io::stdout(), tui).await
}
```

`serve` gains `tui: bool` as its last parameter; its tail becomes:

```rust
    wait.stop(true).await;
    // The api hands out the /tunnel URL; the TUI door is the same gateway, same token, next route.
    let gateway = if tui { session.gateway.replacen("/tunnel/", "/tui/", 1) } else { session.gateway.clone() };
    let url = crate::proxy::gateway_url(&gateway);
    let ws = crate::proxy::connect(&url, &session.token).await?;
    crate::proxy::pump_io(ws, r, w, tui.then_some(Duration::from_secs(15))).await
```

Existing test callers of `serve` pass `false`.

`main.rs:73`:

```rust
    BenchProxy {
        /// Pump to the bench daemon's TUI port instead of sshd (the laptop kl-tui's direct path)
        #[arg(long)]
        tui: bool,
        team: Option<String>,
    },
```

`main.rs:182`: `Cmd::BenchProxy { team, tui } => bench::proxy(team.as_deref(), *tui).await,`
(drop the `*` if the match binds by value; follow the neighbouring arms).

- [ ] **Step 5: Implement the fallback chain in `bench()`**

Above `bench()`:

```rust
/// How `kl-connect bench` reaches the bench, tried in order; exit 3 from one means "not this way"
/// (an old bench, a refused upgrade, a protocol mismatch) and moves to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// laptop kl-tui over `bench-proxy --tui`: wss end to end, no ssh
    Direct,
    /// laptop kl-tui over ssh to `kl-host` (the clipboard forward rides along)
    LaptopSsh,
    /// the TUI runs on the bench, over `ssh -t`
    RemoteSsh,
}

/// Direct only on request until the gateway hostnames are Cloudflare Full (strict) with the Origin
/// CA certificate: before that, the edge-to-node hop is plain HTTP and a prompt would cross it in
/// clear. Flipping the default is a separate change once that is live.
fn modes(direct: bool, laptop: bool) -> Vec<Mode> {
    match (direct, laptop) {
        (true, true) => vec![Mode::Direct, Mode::LaptopSsh, Mode::RemoteSsh],
        (false, true) => vec![Mode::LaptopSsh, Mode::RemoteSsh],
        (_, false) => vec![Mode::RemoteSsh],
    }
}
```

Replace the block from `let laptop = …` through `let fallback = …; if fallback { … }` with:

```rust
    let laptop = if remote_tui { None } else { kl_tui_beside(&me) };
    let direct = std::env::var("KL_DIRECT").as_deref() == Ok("1");
    let mut st: std::io::Result<std::process::ExitStatus> = Err(std::io::Error::other("no mode"));
    let mut ssh_ran = false;
    for (n, mode) in modes(direct, laptop.is_some()).into_iter().enumerate() {
        if n > 0 {
            restore_terminal(true);
        }
        st = match (mode, &laptop) {
            (Mode::Direct, Some(tui)) => {
                let mut args = vec!["--pipe".to_string(), me.display().to_string(), "bench-proxy".into(), "--tui".into()];
                args.extend(team.map(str::to_string));
                tokio::process::Command::new(tui).args(args).status().await
            }
            (Mode::LaptopSsh, Some(tui)) => {
                let mut args = vec!["--ssh".to_string()];
                args.extend(ssh_argv(&me, &known_hosts, owner, team, fwd, Some("kl-host"), false));
                tokio::process::Command::new(tui).args(args).status().await
            }
            _ => {
                ssh_ran = true;
                tokio::process::Command::new("ssh")
                    .args(ssh_argv(&me, &known_hosts, owner, team, fwd, None, true))
                    .status()
                    .await
            }
        };
        // 3 = not this way (old bench, refused upgrade, other protocol): try the next one
        if !matches!(&st, Ok(s) if s.code() == Some(3)) {
            break;
        }
    }
    let fallback = ssh_ran;
```

The rest of `bench()` (`clip.abort()`, socket removal, `restore_terminal(fallback || …)`, exit)
stays as is. Keep the `// 3 = the bench predates …` intent in the loop comment above.

- [ ] **Step 6: Run, expect pass**

Run: `CARGO_TARGET_DIR=/Volumes/kdisk/target-master cargo test -p kl-connect 2>&1 | grep -E "^test result|FAILED|panicked" | head`
Expected: every `test result: ok`.
Then: `CARGO_TARGET_DIR=/Volumes/kdisk/target-master cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add bins/kl-connect
git commit -m "Connect the laptop TUI to the bench over wss when KL_DIRECT=1"
```

---

### Task 5: kl-tui `--pipe`

**Files:**
- Modify: `harness/apps/tui/src/remote.tsx:1-20`

**Interfaces:**
- Consumes: `kl-connect bench-proxy --tui` (Task 4), invoked as
  `kl-tui --pipe <kl-connect> bench-proxy --tui [team]`.
- Produces: `--pipe <argv…>` runs the argv as given; `--ssh <argv…>` unchanged.

- [ ] **Step 1: Write the failing test**

Create `harness/apps/tui/src/remote-args.test.ts`:

```ts
import { expect, test } from "bun:test";
import { pipeArgv } from "./remote-args.ts";

test("--ssh prefixes ssh", () => {
  expect(pipeArgv(["bun", "kl-tui", "--ssh", "-p", "22", "kl-host"])).toEqual(["ssh", "-p", "22", "kl-host"]);
});
test("--pipe runs the argv as given", () => {
  expect(pipeArgv(["bun", "kl-tui", "--pipe", "/bin/kl-connect", "bench-proxy", "--tui"])).toEqual(["/bin/kl-connect", "bench-proxy", "--tui"]);
});
test("no transport or an empty one is a usage error", () => {
  expect(pipeArgv(["bun", "kl-tui"])).toBeNull();
  expect(pipeArgv(["bun", "kl-tui", "--pipe"])).toBeNull();
});
```

- [ ] **Step 2: Run, expect failure**

Run: `cd harness/apps/tui && bun test src/remote-args.test.ts 2>&1 | tail -3`
Expected: FAIL, cannot find module `./remote-args.ts`.

- [ ] **Step 3: Implement**

Create `harness/apps/tui/src/remote-args.ts`:

```ts
/** kl-tui's transport: `--ssh <ssh argv…>` (ssh is prefixed) or `--pipe <command…>` (run as given —
 * kl-connect's `bench-proxy --tui`, wss to the bench daemon). `null` = usage error. Split from
 * remote.tsx, which runs on import. */
export function pipeArgv(argv: string[]): string[] | null {
  for (const [flag, prefix] of [["--ssh", ["ssh"]], ["--pipe", []]] as const) {
    const i = argv.indexOf(flag);
    if (i >= 0) return i === argv.length - 1 ? null : [...prefix, ...argv.slice(i + 1)];
  }
  return null;
}
```

In `remote.tsx`, replace lines 9-16 (the `indexOf("--ssh")` check and the `connect(["ssh", …])`) with:

```ts
const cmd = pipeArgv(process.argv);
if (!cmd) {
  process.stderr.write("usage: kl-tui --ssh <ssh arguments...> | --pipe <command...>\n");
  process.exit(2);
}
let c: Awaited<ReturnType<typeof connect>>;
try {
  c = await connect(cmd);
```

Import `pipeArgv` from `./remote-args.ts`. Header comment line 2 becomes
`// kl-tui: the TUI on the laptop, the agent on the bench. \`kl-tui --ssh <ssh argv...>\` or
\`kl-tui --pipe <command...>\` (kl-connect's wss pipe); kl-connect builds the argv …`. In `done()`,
the comment `// ssh writes nothing when its proxy dies` becomes `// neither ssh nor the pipe says
anything when the bench goes away`.

- [ ] **Step 4: Run, expect pass**

Run: `cd harness/apps/tui && bun test src/remote-args.test.ts 2>&1 | tail -3`
Expected: 3 pass.

- [ ] **Step 5: Commit**

```bash
git add harness/apps/tui/src/remote-args.ts harness/apps/tui/src/remote-args.test.ts harness/apps/tui/src/remote.tsx
git commit -m "Let kl-tui run its transport as a plain pipe"
```

---

### Task 6: Sync — the session list is pushed (`sessions.watch`)

**Files:**
- Modify: `harness/packages/backend/src/index.ts:142-147` (`Backend.sessions`)
- Modify: `harness/packages/backend/src/local.ts:128-157` (`session()` hooks, `sessions`)
- Modify: `harness/packages/backend/src/serve.ts:48-53`, `:66-71`
- Modify: `harness/packages/backend/src/remote.ts:30-33`, `:85`
- Modify: `harness/apps/tui/src/app.tsx:278-284` (list effect), `:1392` (sidebar `running`)
- Test: `harness/packages/backend/src/local.test.ts`, `harness/packages/backend/src/daemon.test.ts`,
  `harness/apps/tui/src/sync.test.tsx`

**Interfaces:**
- Produces: `Backend.sessions.watch(cb: (list: LiveSessionMeta[]) => void): Promise<() => void>`
  where `export type LiveSessionMeta = SessionMeta & { busy: boolean }` (index.ts). Resolves with an
  off function after calling `cb` once; rejects on a bench without the op. Wire op `sessions.watch`
  (args `null`, returns `true`); event `{ ev: "sessions", key: "*", event: LiveSessionMeta[] }`.

- [ ] **Step 1: Write the failing backend tests**

In `local.test.ts`, beside "sessions.list offers every key" (reuse its `KLOUDLITE_CONFIG_DIR`
setup, model pick and `o`):

```ts
test("sessions.watch answers at once and on every change, and stops after off", async () => {
  const b = new LocalBackend();
  const lists: any[][] = [];
  const off = await b.sessions.watch((l) => lists.push(l));
  expect(lists.length).toBe(1);
  const h = await b.session("main", o);
  expect(lists.length).toBeGreaterThan(1); // opened
  const n = lists.length;
  await b.sessions.name("main", "renamed");
  expect(lists.length).toBe(n + 1);
  expect(lists.at(-1)!.find((m) => m.key === "main")?.name).toBe("renamed");
  await h.dispose();
  await new Promise((r) => setTimeout(r, 10));
  expect(lists.length).toBeGreaterThan(n + 1); // closed
  const m = lists.length;
  off();
  await b.sessions.name("main", "again");
  expect(lists.length).toBe(m);
}, 20000);
```

Turn start and end are reported from `baseHandle`'s busy hooks, so they get a unit test with the
existing `fakeAgent()` helper (near line 211; its `emit` fires every subscriber):

```ts
test("baseHandle reports turn start and end to its change hook", () => {
  const agent = fakeAgent();
  let changes = 0;
  baseHandle(agent, "main", { busy: new Set(), onEnd() {}, onDispose() {}, onChange: () => void changes++ });
  agent.emit({ type: "agent_start" });
  agent.emit({ type: "agent_end", messages: [] });
  expect(changes).toBe(2);
});
```

Name the hook `onChange` exactly as here.

In `daemon.test.ts`:

```ts
test("sessions.watch over the wire: answered at once, pushed after; an old bench rejects", async () => {
  let cb: ((l: any[]) => void) | undefined;
  const { b } = fakeBackend();
  (b as any).sessions = { watch: async (f: any) => ((cb = f), f([{ key: "main", busy: false }]), () => {}) };
  const server = await listenTcp(b, 0, "127.0.0.1");
  const c = tcpClient((server.address() as any).port);
  const got: any[][] = [];
  await c.backend.sessions.watch((l) => got.push(l));
  await new Promise((r) => setTimeout(r, 50));
  cb!([{ key: "main", busy: true }]);
  await new Promise((r) => setTimeout(r, 50));
  expect(got.map((l) => l[0].busy)).toEqual([false, true]);
  c.close();
  server.close();

  const old = await listenTcp(fakeBackend().b, 0, "127.0.0.1"); // no sessions.watch handler
  const o = tcpClient((old.address() as any).port);
  await expect(o.backend.sessions.watch(() => {})).rejects.toThrow();
  o.close();
  old.close();
});
```

- [ ] **Step 2: Run, expect failure**

Run: `cd harness/packages/backend && bun test src/local.test.ts src/daemon.test.ts 2>&1 | tail -6`
Expected: FAIL (`watch` is not a function; `onChange` unknown).

- [ ] **Step 3: Implement the backend**

`index.ts`, in `Backend.sessions`:

```ts
    /** Pushed list with live busy: `cb` runs once before this resolves, then on every open,
     * close, turn start, turn end, name, describe and clear. Rejects on a bench without the op
     * (the caller keeps fetching `list`). Resolves with the unsubscribe. */
    watch(cb: (list: LiveSessionMeta[]) => void): Promise<() => void>;
```

and beside `SessionMeta`: `export type LiveSessionMeta = SessionMeta & { busy: boolean };`.

`local.ts` `baseHandle` hooks type gains `onChange?(): void`; in its `agent.subscribe` callback,
after the busy add/delete lines:

```ts
    if (event.type === "agent_start" || event.type === "agent_end") hooks.onChange?.();
```

`LocalBackend`:

```ts
  #watchers = new Set<(list: LiveSessionMeta[]) => void>();
  /** Every view's sidebar: the stored list with which keys are mid-turn right now. */
  #changed() {
    if (this.#watchers.size === 0) return;
    const list = listSessions().map((m) => ({ ...m, busy: this.#busy.has(m.key) }));
    for (const cb of [...this.#watchers]) {
      try {
        cb(list);
      } catch (err) {
        console.error("sessions watcher failed", err);
      }
    }
  }
```

In `session()`: pass `onChange: () => this.#changed()` in the `baseHandle` hooks; call
`this.#changed()` at the end of `onDispose` (after the deletes) and right after
`this.#shared.set(key, made);`.

`sessions`:

```ts
  sessions = {
    list: async (prefix?: string) => listSessions(prefix),
    name: async (key: string, name: string) => (nameSession(key, name), this.#changed()),
    describe: async (key: string, d: string) => (describeSession(key, d), this.#changed()),
    clear: async (key: string) => (clearSessionHistory(key), this.#changed()),
    watch: async (cb: (list: LiveSessionMeta[]) => void) => {
      this.#watchers.add(cb);
      cb(listSessions().map((m) => ({ ...m, busy: this.#busy.has(m.key) })));
      return () => void this.#watchers.delete(cb);
    },
  };
```

`#changed` is a private method used inside a class-field initializer; class fields initialize in
order after private methods exist, so this is fine. If `nameSession` etc. are async, `await` them
before `this.#changed()`.

`serve.ts`: the generic loop skips `watch`, and `watch` gets its own handler:

```ts
    for (const [name, fn] of Object.entries(ops)) {
      if (group === "sessions" && name === "watch") continue; // a stream, not a call: below
      peer.handle(`${group}.${name}`, (args: unknown[]) => (fn as any)(...args));
    }
```

```ts
  // One watch per connection, however often the client asks; the list goes out as an event.
  let unwatch: (() => void) | undefined;
  peer.handle("sessions.watch", async () => {
    unwatch ??= await backend.sessions.watch((list) => peer.emit("sessions", "*", list));
    return true;
  });
```

and `dispose()` calls `unwatch?.()` first.

`remote.ts`: add `#watchers = new Set<(l: any[]) => void>();`; `onEvent` gains
`else if (ev === "sessions") for (const cb of this.#watchers) cb(event as any);`; `sessions` becomes:

```ts
  sessions: Backend["sessions"] = {
    ...this.#ops<Omit<Backend["sessions"], "watch">>("sessions", ["list", "name", "describe", "clear"]),
    watch: async (cb) => {
      this.#watchers.add(cb);
      try {
        await this.peer.request("sessions.watch", null);
      } catch (e) {
        this.#watchers.delete(cb); // an old bench: "unknown op"
        throw e;
      }
      return () => void this.#watchers.delete(cb);
    },
  };
```

The daemon emits the first list before the request resolves; the watcher is registered before
the request is sent, so that first list is not lost.

- [ ] **Step 4: Run, expect pass**

Run: `cd harness/packages/backend && bun test 2>&1 | grep -E "pass|fail" | tail -3`
Expected: 0 fail.

- [ ] **Step 5: Write the failing TUI tests**

Create `harness/apps/tui/src/sync.test.tsx`. It drives `App` against a LocalBackend whose
`sessions` ops are scripted, so the test plays the bench answering (or not answering) the watch:

```tsx
import { expect, test } from "bun:test";
import { testRender } from "@opentui/react/test-utils";
import { LocalBackend } from "@kloudlite-tui/backend/local";
import { boot, hello } from "./hello.ts";
import { App } from "./app.tsx";

const tick = () => new Promise((r) => setTimeout(r, 60));

/** `watch: "old"` plays a bench without the op; `push` plays the bench pushing a new list. */
function scripted(opts: { watch?: "answers" | "old" } = {}) {
  const b: any = new LocalBackend();
  const listCalls: string[] = [];
  let watcher: ((l: any[]) => void) | undefined;
  b.sessions = {
    list: async (prefix: string) => (listCalls.push(prefix), []),
    name: async () => {},
    describe: async () => {},
    clear: async () => {},
    watch: async (cb: any) => {
      if (opts.watch === "old") throw new Error("unknown op: sessions.watch");
      watcher = cb;
      cb([]);
      return () => {};
    },
  };
  return { b, listCalls, push: (l: any[]) => watcher?.(l) };
}

async function mount(b: any) {
  boot(b, { ...hello(), settings: { ...hello().settings, vim: "off", sidebarWidth: 42 } });
  const setup = await testRender(<App />, { width: 200, height: 32, kittyKeyboard: true });
  const frame = async () => (await tick(), await setup.renderOnce(), setup.captureCharFrame());
  await frame();
  return { ...setup, frame, done: () => setup.renderer.destroy() };
}

test("a session another view names shows up here without a fetch", async () => {
  const s = scripted({ watch: "answers" });
  const ui = await mount(s.b);
  s.push([{ key: "main/notes", name: "release notes", busy: true }]);
  expect(await ui.frame()).toContain("release notes");
  ui.done();
});

test("a bench that answers sessions.watch is never asked for the list", async () => {
  const s = scripted({ watch: "answers" });
  const ui = await mount(s.b);
  await ui.frame();
  expect(s.listCalls).toEqual([]);
  ui.done();
});

test("an old bench without sessions.watch keeps fetching the list", async () => {
  const s = scripted({ watch: "old" });
  const ui = await mount(s.b);
  await ui.frame();
  expect(s.listCalls.length).toBeGreaterThan(0);
  ui.done();
});
```

Run: `cd harness/apps/tui && bun test src/sync.test.tsx 2>&1 | tail -4`
Expected: the first FAILS (`list` is called today).

- [ ] **Step 6: Implement the TUI side**

`app.tsx`, replace lines 281-284 (`const [baseSessions, …]` and its effect) with:

```tsx
  // Pushed by the bench when it can (`sessions.watch`): every view's list and busy marks move the
  // moment any view opens, names or runs a session. `undefined` = not answered yet, `null` = an
  // old bench, which keeps today's fetch.
  const [watched, setWatched] = useState<LiveSessionMeta[] | null | undefined>(undefined);
  useEffect(() => {
    let off: (() => void) | undefined;
    let gone = false;
    backend()
      .sessions.watch((l) => !gone && setWatched(l))
      .then((f) => (gone ? f() : (off = f)))
      .catch(() => !gone && setWatched(null));
    return () => {
      gone = true;
      off?.();
    };
  }, []);
  const [fetched, setFetched] = useState<SessionMeta[]>([]);
  useEffect(() => {
    if (watched !== null) return;
    backend().sessions.list(activeBase).then(setFetched).catch(() => {});
  }, [watched, activeBase, sessionNames, sessionDescs]);
  // same filter as the bench's listSessions(prefix)
  const baseSessions = watched ? watched.filter((m) => m.key.startsWith(activeBase)) : fetched;
```

Import `type LiveSessionMeta` from `@kloudlite-tui/backend` beside `SessionMeta`.

Sidebar, line 1392:

```tsx
      running={workspaces.map((w) => getSession(sessions, w.id).busy || !!watched?.find((m) => m.key === w.id)?.busy)}
```

- [ ] **Step 7: Run, expect pass**

Run: `cd harness/apps/tui && bun test 2>&1 | grep -E "pass|fail" | tail -3`
Expected: 0 fail.

- [ ] **Step 8: Commit**

```bash
git add harness/packages/backend/src harness/apps/tui/src
git commit -m "Push the session list to every view"
```

---

### Task 7: Live drill on a bench the owner restarts

Nothing here is automated; each step records its evidence (command and the decisive output line)
in the ledger. The bench pod is restarted by the owner only.

**Files:** none changed. Ship per CLAUDE.md "Deploying" (build from a clean detached worktree):
gateway image, bench image (daemon + kl-tui), kl-connect build; pin; roll. Bench pod restart:
ask the owner.

- [ ] **Step 1: NetworkPolicy fence.** From a throwaway pod in another namespace, dial the bench pod
IP on 7791 (`nc -z -w3 <ip> 7791`); expect a timeout. Note: the AKS cluster has no policy engine
(memory `aks-no-network-policy-engine`); on that cluster record "skipped: no engine", and run it on
k3s where the bench lives.
- [ ] **Step 2: Direct + ttyd on one session.** `KL_DIRECT=1 kl-connect bench` on the laptop; open
the ttyd TUI in a browser. Start a turn in one; the other's sidebar marks it busy, then idle when it ends. Name a session in
one; the other's list shows the name.
- [ ] **Step 3: Idle.** Leave the direct session idle for 5 minutes; it is still connected (keepalive
beats the 100 s edge).
- [ ] **Step 4: `KL_DIRECT=0 kl-connect bench`** connects over ssh as today.
- [ ] **Step 5: Old bench.** Against a bench still on the previous image, `KL_DIRECT=1 kl-connect
bench` falls back (502 from the gateway, exit 3) and connects over ssh.
- [ ] **Step 6: Ledger and board.** Record each step's evidence; update the status board row.
