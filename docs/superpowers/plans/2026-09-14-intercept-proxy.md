# Intercept Proxy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An intercepted environment service is backed by a small TCP forwarder pod **in the environment's namespace** instead of a hand-written `EndpointSlice` naming a workspace pod IP in another namespace. The ClusterIP keeps its selector, the endpoints become Kubernetes' own, and every follower of the environment (every space that points at it) reaches the intercepted service — which is the owner's bug: today a teammate's workspace or bench in `ws-bob` gets its packets dropped by `ws-alice`'s ingress policy, silently, on every intercept since spaces shipped.

**Architecture:** Three pieces.

1. A new binary `bins/intercept-proxy` → `kloudlite-intercept-proxy`: musl static, tokio, one `TcpListener` per forwarded port, `copy_bidirectional` and nothing else. Its own `FROM scratch` Dockerfile stage, its own ghcr image, pinned by `deploy/pin.sh` into the agent DaemonSet's env the way `kloudlite-workspace` is (the agent hands it to a tenant-namespace pod; it is not a workload of ours).
2. A **pure render module** `crates/workspaces/src/k8s/intercept.rs`: given a service, an intercept, the workspace and its namespace, it returns the object set an in-force intercept needs — the proxy Pod, the workspace-side target Service, the narrowed egress policy — plus the names a release deletes. No `kube::Api`, no `async`, testable with `cargo test -p kloudlite-workspaces` and no cluster.
3. The caller: today's agent environment reconciler (`bins/agent/src/controller/environment/run.rs` phase 4 + `intercept.rs`), which stops writing the slice and writes the object set instead.

**Ownership note — read this before starting.** The region controller (`docs/superpowers/specs/2026-09-14-region-controller-design.md`) is being designed in parallel and **does not exist yet**. This plan deliberately puts every decision that is not "which API object" into the pure module in `crates/workspaces`, and leaves the agent's environment reconciler as the only thing that calls it. When the region controller lands, moving the proxy to it is a change to the CALLER only — the render module, its tests, the CRD field and the RBAC all move unchanged. Do not wait for the region controller: this ships and fixes the owner's bug now.

**Spec:** `docs/superpowers/specs/2026-09-14-intercept-proxy-design.md` (commit `0c5ca8ae`). It supersedes §3 of `2026-09-08-service-intercept-design.md`; everything else in that spec — the wish in `Environment.spec.intercepts`, one workspace per service, the grace, what removes a wish — stands unchanged and is not touched here.

## Rulings on the spec's open questions

These are decided. Do not re-open them; each carries its one-line why.

1. **The proxy is a bare Pod owned by the Environment**, recreated by the controller's own pass, `restartPolicy: Always`. A Deployment adds a ReplicaSet to reason about and nothing else — the controller is already the thing that repairs a missing proxy, and it repairs one on the same beat it repairs everything else.
2. **`--idle-secs` is a compiled-in constant (600).** It is a guess nobody has complained about; a `ClusterSettings` field for a value no one has tuned is config for a constant. Marked `// ponytail: fixed 600s idle; a ClusterSettings Mark::Live field if a long-poll is ever reported cut`.
3. **One proxy pod per intercepted SERVICE.** A workspace serving two of an environment's services gets two proxies. Ownership and release are then per service, exactly like the ClusterIP and the StatefulSet they sit between; one pod per workspace would make a release of one service a mutation of a pod another service depends on.
4. **Nothing reads the intercepted Service's `Endpoints` object.** The spec author verified this by grep; no dashboard, no debug path, no probe. So `drop_abandoned_endpoints` and the legacy `Endpoints` delete go — with the selector kept, Kubernetes never abandons anything again.
5. **Target addressing is the spec's option (b)**: a ClusterIP `intercept-target-{ws}` in the workspace namespace selecting `WORKSPACE_LABEL`. Baking the pod IP into args misdelivers to another tenant between a workspace restart and the next pass.

## Global Constraints

- **No payload ever reaches a log.** The forwarder logs accept/close/error with the peer address, byte counts and the port — never bytes, never a decoded frame. It parses nothing.
- **Order, taking force**: target Service → proxy Pod → proxy Ready → selector switches to the proxy → StatefulSet to 0. **Order, releasing**: selector back to the StatefulSet's labels → replicas back → delete proxy Pod → delete target Service. The service must never be without a ready endpoint, in either direction. A not-Ready proxy leaves the real service serving.
- **Every delete is followed by `forget_applied`** for the same kind/namespace/name (the apply-hash cache would otherwise skip the recreate). This is the F1/F3 class the space vetting already found; it is not optional and it is asserted in the reconcile tests.
- `Intercepting::Keep` renders exactly what the last pass rendered, including the proxy, the target Service and both grants. An unreadable API answer must never flap a service.
- A reflector cache that has not finished its first list is UNKNOWN, never empty (`Ctx::workspaces()` → `Keep`). Unchanged by this plan; do not weaken it.
- `crates/workspaces` never calls the API server. The render module returns objects; `bins/agent` applies them.
- House style: `//!` module docs carry the why, files stay under ~800 lines, `// ponytail:` marks a ceiling with its upgrade path, comments explain WHY. Commit subjects imperative sentence case, **no attribution lines**. Build/test/ship per `CLAUDE.md` "Dev loop"; `cargo clippy --workspace --all-targets -- -D warnings` gates CI.
- One commit per task, pushed to `platform` after each. Other agents commit in this worktree: `git log -3` before every commit, and stage only the files the task names.

---

## File Structure

| Path | Responsibility |
|---|---|
| `bins/intercept-proxy/Cargo.toml` (new) | `kloudlite-intercept-proxy` package |
| `bins/intercept-proxy/src/main.rs` (new) | arg parse, bind, accept loop, `main` |
| `bins/intercept-proxy/src/lib.rs` (new) | `Args`, `parse`, `serve_one`, `pump` — the testable half |
| `bins/intercept-proxy/tests/forward.rs` (new) | round trip, remap, half-close, idle, bound |
| `Dockerfile` | `FROM scratch AS intercept-proxy` stage |
| `.github/workflows/image.yml` | musl build, artifact, chmod, build-push |
| `deploy/pin.sh` | eighth image, pinned into `k3s/agent-daemonset.yaml` |
| `deploy/k3s/agent-daemonset.yaml` | `WS_INTERCEPT_PROXY_IMAGE` env |
| `crates/workspaces/src/settings.rs` | `intercept_proxy_image` on `AgentSettings` |
| `crates/workspaces/src/crd/settings.rs` | `interceptProxyImage` on `ClusterSettingsSpec` (Boot) |
| `crates/workspaces/src/k8s/intercept.rs` (new) | the pure render: names + `ProxyRender` |
| `crates/workspaces/src/k8s/mod.rs` | `mod intercept; pub use intercept::*;` |
| `crates/workspaces/src/k8s/environment.rs` | `service_clusterip` takes the proxy's labels; `intercept_slice` deleted in Task 5 |
| `crates/workspaces/src/k8s/policies.rs` | `intercept_egress` narrowed to the proxy pod |
| `crates/workspaces/src/k8s/tests/intercept.rs` (new) | render tests |
| `crates/workspaces/src/crd/environment.rs` | `ServiceStatus::proxy` |
| `bins/agent/src/controller/mod.rs` | `Ctx::intercept_proxy_image` |
| `bins/agent/src/controller/environment/run.rs` | phase 4 rewritten; phase 5a records `proxy` |
| `bins/agent/src/controller/environment/intercept.rs` | `apply_intercept` / `release_intercept` |
| `bins/agent/src/controller/environment/services.rs` | `drop_abandoned_endpoints` deleted |
| `bins/agent/tests/reconcile/intercept_proxy.rs` (new) | order, release, not-ready, forget |
| `crates/workspaces/src/api/environments.rs` | UDP 422 |
| `bins/slo/src/stages/environment.rs` | the nine-id intercept journey |
| `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md` | the new ids |
| `deploy/k3s/agent-rbac.yaml` | table rows + rules |
| `CLAUDE.md` | the intercept paragraph rewritten |

---

### Task 1: The forwarder binary

**Files:**
- Create: `bins/intercept-proxy/Cargo.toml`, `bins/intercept-proxy/src/lib.rs`, `bins/intercept-proxy/src/main.rs`, `bins/intercept-proxy/tests/forward.rs`
- Modify: `Cargo.toml` (`members`, `default-members`)

**Interfaces:**
- Consumes: nothing.
- Produces (used by nothing in Rust — the contract is the argv, which Task 3 renders):
  - `kloudlite_intercept_proxy::Args { target: String, forwards: Vec<(u16, u16)>, max_conns: usize }`
  - `kloudlite_intercept_proxy::parse(argv: impl Iterator<Item = String>) -> Result<Args, String>`
  - `kloudlite_intercept_proxy::serve(args: Args) -> anyhow-free Result<(), String>` (binds every listener before serving any)
  - CLI: `kloudlite-intercept-proxy --target <host> --forward <listen>:<target_port> [--forward …] [--max-conns N]`

**Steps:**

- [ ] 1. Create `bins/intercept-proxy/Cargo.toml`:
```toml
[package]
name = "kloudlite-intercept-proxy"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"

[lib]
name = "kloudlite_intercept_proxy"
path = "src/lib.rs"

[[bin]]
name = "kloudlite-intercept-proxy"
path = "src/main.rs"

# Deliberately three dependencies. This binary is in the data path of every intercepted service;
# every crate here is one more thing that can hold a connection open wrong.
[dependencies]
tokio = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```
  Add `"bins/intercept-proxy"` to both `members` and `default-members` in the root `Cargo.toml`, after `"bins/kl"`.

- [ ] 2. `bins/intercept-proxy/src/lib.rs`. Module doc first: what it is (a byte-for-byte TCP forwarder that stands in for an intercepted environment service), why it exists (endpoints must live in the environment's namespace so every following space reaches them), and what it must never do (parse, log payload, resolve the target once). Then:

```rust
//! … (as above)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// No bytes in either direction for this long closes the connection.
///
/// Workspace pods restart often; without a deadline every restart leaks one half-open socket per
/// port until the proxy itself dies.
///
/// ponytail: fixed 600s idle; a ClusterSettings `Mark::Live` field is the upgrade path if a
/// long-poll is ever reported cut.
pub const IDLE_SECS: u64 = 600;

/// Default ceiling on in-flight connections. A workspace dev server is not a fleet service, and an
/// unbounded accept loop turns one loop in a caller into an OOM in the environment's namespace.
pub const DEFAULT_MAX_CONNS: usize = 512;

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    /// The DNS name of the workspace-side target Service. An FQDN, because this pod's resolv.conf
    /// is the ENVIRONMENT namespace's and its search path does not contain the workspace's.
    pub target: String,
    /// `(listen_port, target_port)`, one per declared service port. The remap lives here.
    pub forwards: Vec<(u16, u16)>,
    pub max_conns: usize,
}

pub fn parse(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut target = None;
    let mut forwards = Vec::new();
    let mut max_conns = DEFAULT_MAX_CONNS;
    let mut it = argv;
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--target" => target = Some(val()?),
            "--max-conns" => max_conns = val()?.parse().map_err(|_| "--max-conns is not a number".to_string())?,
            "--forward" => {
                let v = val()?;
                let (l, t) = v.split_once(':').ok_or_else(|| format!("--forward {v} is not listen:target"))?;
                let l: u16 = l.parse().map_err(|_| format!("--forward {v}: {l} is not a port"))?;
                let t: u16 = t.parse().map_err(|_| format!("--forward {v}: {t} is not a port"))?;
                if l == 0 || t == 0 {
                    return Err(format!("--forward {v}: 0 is not a port"));
                }
                forwards.push((l, t));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let target = target.ok_or("--target is required")?;
    if forwards.is_empty() {
        return Err("at least one --forward is required".into());
    }
    Ok(Args { target, forwards, max_conns })
}

/// Bind EVERY listener before serving any.
///
/// A half-bound proxy that answers on 8080 and refuses 9229 is worse than a pod that will not
/// start: the Service would go Ready with half its ports dead, and the person would be debugging
/// their own app. A bind failure is fatal, deliberately.
pub async fn bind_all(args: &Args) -> Result<Vec<(TcpListener, u16)>, String> {
    let mut out = Vec::new();
    for (listen, to) in &args.forwards {
        let l = TcpListener::bind(("0.0.0.0", *listen)).await.map_err(|e| format!("bind 0.0.0.0:{listen}: {e}"))?;
        out.push((l, *to));
    }
    Ok(out)
}

pub async fn serve(args: Args) -> Result<(), String> {
    let listeners = bind_all(&args).await?;
    let limit = Arc::new(Semaphore::new(args.max_conns));
    let target = Arc::new(args.target);
    let mut tasks = Vec::new();
    for (l, to) in listeners {
        tasks.push(tokio::spawn(accept_loop(l, to, target.clone(), limit.clone())));
    }
    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

pub async fn accept_loop(l: TcpListener, to: u16, target: Arc<String>, limit: Arc<Semaphore>) {
    loop {
        // The permit is taken BEFORE the accept, so over the bound the loop waits in the kernel's
        // backlog rather than spawning a task per pending connection.
        let Ok(permit) = limit.clone().acquire_owned().await else { return };
        let (sock, peer) = match l.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, port = to, "proxy.accept_failed");
                continue;
            }
        };
        let target = target.clone();
        tokio::spawn(async move {
            let _permit = permit;
            // Resolved PER CONNECTION, never once at boot: the target Service's ClusterIP is
            // stable, but a workspace recreate that recreates the Service must not need this pod
            // restarted to be reachable.
            match TcpStream::connect((target.as_str(), to)).await {
                Ok(up) => {
                    if let Err(e) = pump(sock, up).await {
                        tracing::debug!(error = %e, %peer, port = to, "proxy.closed_with_error");
                    }
                }
                Err(e) => tracing::warn!(error = %e, %peer, port = to, "proxy.dial_failed"),
            }
        });
    }
}

/// The whole data path: copy both directions, half-close each as its source ends, give up after
/// `IDLE_SECS` with no bytes at all.
///
/// `copy_bidirectional` shuts each direction down as its source ends, which is what a client that
/// half-closes to signal end-of-request (plain HTTP/1.0, some RPC framings) needs; without it such
/// a client hangs forever. NOTHING here looks at the bytes.
pub async fn pump(mut client: TcpStream, mut upstream: TcpStream) -> std::io::Result<(u64, u64)> {
    let copy = tokio::io::copy_bidirectional(&mut client, &mut upstream);
    match tokio::time::timeout(Duration::from_secs(IDLE_SECS), copy).await {
        Ok(r) => r,
        Err(_) => {
            let _ = client.shutdown().await;
            let _ = upstream.shutdown().await;
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "idle"))
        }
    }
}

/// Only used to keep `HashMap` in scope for future port maps; delete if unused.
#[allow(dead_code)]
type Unused = HashMap<u16, u16>;
```
  Delete the `Unused` alias and the `HashMap` import if nothing needs them — do not ship a dead type.

  **Ceiling to mark**: `IDLE_SECS` is a whole-connection deadline, not a since-last-byte one — a single connection that legitimately runs longer than 600 s is cut. Add exactly this comment on the `timeout`:
```rust
// ponytail: this is a whole-connection deadline, not an idle one — a legitimate connection
// open past IDLE_SECS is cut. A `Instant` last-byte tracker wrapped around the two halves is
// the upgrade path; nothing in an environment holds a connection that long today.
```

- [ ] 3. `bins/intercept-proxy/src/main.rs`:
```rust
//! `kloudlite-intercept-proxy`: the forwarder behind an intercepted environment service.

fn main() {
    tracing_subscriber::fmt().with_target(false).json().init();
    let args = match kloudlite_intercept_proxy::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kloudlite-intercept-proxy: {e}");
            std::process::exit(2);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    // A bind failure exits non-zero; Kubernetes restarts the pod and the Service stays without a
    // ready endpoint, which is the honest answer. See `bind_all`.
    if let Err(e) = rt.block_on(kloudlite_intercept_proxy::serve(args)) {
        eprintln!("kloudlite-intercept-proxy: {e}");
        std::process::exit(1);
    }
}
```

- [ ] 4. `bins/intercept-proxy/tests/forward.rs` — the whole check. A stub target on a loopback port; no framework, no fixtures:
```rust
//! The forwarder against a stub target on loopback. Nothing here needs a cluster.

use kloudlite_intercept_proxy::{parse, pump, Args, DEFAULT_MAX_CONNS};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A target that echoes every byte back and then closes when its peer half-closes.
async fn echo_target() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => {
                            let _ = s.shutdown().await;
                            return;
                        }
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    port
}

/// Start the forwarder on an ephemeral listen port forwarding to `to`, and answer its port.
async fn proxy_to(to: u16) -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = l.local_addr().unwrap().port();
    let limit = Arc::new(tokio::sync::Semaphore::new(DEFAULT_MAX_CONNS));
    tokio::spawn(kloudlite_intercept_proxy::accept_loop(l, to, Arc::new("127.0.0.1".to_string()), limit));
    port
}

#[test]
fn args_carry_the_remap_and_refuse_nonsense() {
    let a = parse(["--target", "t.ws-alice.svc", "--forward", "8080:3000", "--forward", "9229:9229"].iter().map(|s| s.to_string())).unwrap();
    assert_eq!(a, Args { target: "t.ws-alice.svc".into(), forwards: vec![(8080, 3000), (9229, 9229)], max_conns: DEFAULT_MAX_CONNS });
    assert!(parse(["--target", "t"].iter().map(|s| s.to_string())).is_err(), "no --forward must be refused");
    assert!(parse(["--forward", "8080:3000"].iter().map(|s| s.to_string())).is_err(), "no --target must be refused");
    assert!(parse(["--target", "t", "--forward", "8080:0"].iter().map(|s| s.to_string())).is_err(), "0 is not a port");
}

#[tokio::test]
async fn bytes_round_trip_through_the_remapped_port() {
    let target = echo_target().await;
    let listen = proxy_to(target).await;
    let mut c = TcpStream::connect(("127.0.0.1", listen)).await.unwrap();
    c.write_all(b"hello intercept").await.unwrap();
    let mut buf = [0u8; 15];
    c.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello intercept");
}

#[tokio::test]
async fn a_half_close_reaches_the_target_and_the_answer_comes_back() {
    // The HTTP/1.0 shape: write a request, half-close, read until EOF. Without half-close
    // propagation the echo target never sees EOF and this hangs.
    let target = echo_target().await;
    let listen = proxy_to(target).await;
    let mut c = TcpStream::connect(("127.0.0.1", listen)).await.unwrap();
    c.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
    c.shutdown().await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut out)).await.expect("half-close did not propagate").unwrap();
    assert_eq!(out, b"GET / HTTP/1.0\r\n\r\n");
}

#[tokio::test]
async fn an_idle_connection_is_dropped_at_the_deadline() {
    // `pump` directly with a paused clock — the real IDLE_SECS is ten minutes and a test that
    // waits it out is ten minutes of CI for one assertion.
    tokio::time::pause();
    let target = echo_target().await;
    let a = TcpStream::connect(("127.0.0.1", target)).await.unwrap();
    let b = TcpStream::connect(("127.0.0.1", target)).await.unwrap();
    let h = tokio::spawn(pump(a, b));
    tokio::time::advance(std::time::Duration::from_secs(kloudlite_intercept_proxy::IDLE_SECS + 1)).await;
    let e = h.await.unwrap().expect_err("an idle connection must be dropped");
    assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn the_semaphore_bounds_connections_in_flight() {
    let target = echo_target().await;
    let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let listen = l.local_addr().unwrap().port();
    let limit = Arc::new(tokio::sync::Semaphore::new(1));
    tokio::spawn(kloudlite_intercept_proxy::accept_loop(l, target, Arc::new("127.0.0.1".to_string()), limit.clone()));
    let mut held = TcpStream::connect(("127.0.0.1", listen)).await.unwrap();
    held.write_all(b"x").await.unwrap();
    let mut one = [0u8; 1];
    held.read_exact(&mut one).await.unwrap();
    // The one permit is taken; a second connection is accepted by the kernel but not served until
    // the first closes, so a read on it times out.
    let mut second = TcpStream::connect(("127.0.0.1", listen)).await.unwrap();
    second.write_all(b"y").await.unwrap();
    let mut buf = [0u8; 1];
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), second.read_exact(&mut buf)).await.is_err(),
        "a connection over the bound must wait, not be served"
    );
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(5), second.read_exact(&mut buf)).await.expect("the bound never released").unwrap();
    assert_eq!(&buf, b"y");
}
```

- [ ] 5. Run and fix until green:
```sh
cargo test -p kloudlite-intercept-proxy
cargo clippy -p kloudlite-intercept-proxy --all-targets -- -D warnings
```

- [ ] 6. `git log -3`, then `git add bins/intercept-proxy Cargo.toml Cargo.lock` and commit: `Add the intercept proxy forwarder`.

---

### Task 2: Image, CI and pin plumbing

**Files:**
- Modify: `Dockerfile`, `.github/workflows/image.yml`, `deploy/pin.sh`, `deploy/k3s/agent-daemonset.yaml`, `crates/workspaces/src/settings.rs`, `crates/workspaces/src/crd/settings.rs`, `bins/agent/src/controller/mod.rs`, `bins/agent/src/testsupport.rs`, `deploy/k3s/crds.yaml` (regenerated)

**Interfaces:**
- Consumes: Task 1's binary name.
- Produces: `ghcr.io/kloudlite/kloudlite-intercept-proxy:<sha>@sha256:…`; `Ctx::intercept_proxy_image: String`; `AgentSettings::intercept_proxy_image`; `ClusterSettingsSpec::intercept_proxy_image` (`interceptProxyImage`, **Boot**).

**Steps:**

- [ ] 1. `Dockerfile`, after the `builder-gate` stage:
```dockerfile
# The intercept proxy. `scratch`, not bookworm-slim like its neighbours: the binary is a static
# musl build that opens no file, resolves DNS through the kernel's own getaddrinfo-free path in
# std's resolver, and talks to nothing but two TCP sockets — so there is no libc, no CA bundle and
# no shell for it to need. `USER` is not set here: the pod spec runs it as uid 1000 with a
# read-only root, and a scratch image has no /etc/passwd to name an account in.
FROM scratch AS intercept-proxy
ARG PROFILE=release
COPY target/x86_64-unknown-linux-musl/${PROFILE}/kloudlite-intercept-proxy /kloudlite-intercept-proxy
ENTRYPOINT ["/kloudlite-intercept-proxy"]
```

- [ ] 2. `.github/workflows/image.yml`:
  - the musl build step becomes
    `cargo build --release --locked -p kl -p kloudlite-intercept-proxy --target x86_64-unknown-linux-musl`
    (one `rustup target add`, one invocation — both binaries are musl for the same reason: a base image with no glibc).
  - the `kl-musl` artifact gains `/ci-target/x86_64-unknown-linux-musl/release/kloudlite-intercept-proxy`.
  - the image job's `chmod +x` line gains `target/x86_64-unknown-linux-musl/release/kloudlite-intercept-proxy`.
  - a `docker/build-push-action` step after the builder-gate one, `target: intercept-proxy`, tags `ghcr.io/kloudlite/kloudlite-intercept-proxy:latest` and `:${{ github.sha }}`, with the comment: *"The forwarder behind an intercepted service. Same commit as the agent that renders its pod: the argv is a contract between the two."*

- [ ] 3. `deploy/pin.sh`: add `kloudlite-intercept-proxy` to the `for img in …` list, and after the workspace pin:
```sh
# The proxy image is not a workload of ours either: the agent hands it to a pod in a tenant
# namespace (WS_INTERCEPT_PROXY_IMAGE), so it lives in the DaemonSet's env, not an `image:` line.
pin 'kloudlite-intercept-proxy' "$SHA" "${DIGEST[kloudlite-intercept-proxy]}" k3s/agent-daemonset.yaml
```
  Update the CONTRACT comment's image count at the top of the file to match.

- [ ] 4. `deploy/k3s/agent-daemonset.yaml`: beside `WS_DEFAULT_IMAGE`, add
```yaml
            # The forwarder that stands in for an intercepted service. Pinned by deploy/pin.sh.
            - name: WS_INTERCEPT_PROXY_IMAGE
              value: ghcr.io/kloudlite/kloudlite-intercept-proxy:latest
```
  (`pin.sh` rewrites the tag on the next run.)

- [ ] 5. `crates/workspaces/src/settings.rs`: add `pub intercept_proxy_image: String` to `AgentSettings`, read it as `std::env::var("WS_INTERCEPT_PROXY_IMAGE").unwrap_or_default()` in the env constructor, and add `over!(intercept_proxy_image);` in `merged_with` beside `default_image`.

- [ ] 6. `crates/workspaces/src/crd/settings.rs`: add to `ClusterSettingsSpec`, beside `default_image`:
```rust
    /// The forwarder image an intercepted service's proxy pod runs. **Boot** — read at pod-render
    /// time; a change rolls `kloudlite-agent`. `None` = keep today's env value, so an admin who
    /// never opens this row cannot blank a required image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intercept_proxy_image: Option<String>,
```
  Add the matching `CLUSTER_SETTING_META` row (`Mark::Boot`, reader `kloudlite-agent`) exactly where `defaultImage`'s row is — grep `defaultImage` in `crates/workspaces/src/api/admin/` and mirror every occurrence.

- [ ] 7. `bins/agent/src/controller/mod.rs`: `pub intercept_proxy_image: String` on `Ctx`, filled from `merged.intercept_proxy_image` beside `default_image`. **Do NOT panic on empty**, unlike `default_image`: a region that has not rolled its DaemonSet yet must keep reconciling everything else. Instead, Task 5 treats an empty image as "cannot render a proxy" and settles the intercept `Off`/`ProxyImageUnset` with that sentence. Add the comment saying so.
  `bins/agent/src/testsupport.rs`: `std::env::set_var("WS_INTERCEPT_PROXY_IMAGE", "ghcr.io/kloudlite/kloudlite-intercept-proxy:deadbeef");` beside the existing one.

- [ ] 8. Regenerate the CRD yaml and run:
```sh
CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml
cargo test -p kloudlite-workspaces
cargo clippy --workspace --all-targets -- -D warnings
```

- [ ] 9. `git log -3`, stage the files above, commit: `Build and pin the intercept proxy image`.

---

### Task 3: The pure render module

**Files:**
- Create: `crates/workspaces/src/k8s/intercept.rs`, `crates/workspaces/src/k8s/tests/intercept.rs`
- Modify: `crates/workspaces/src/k8s/mod.rs`, `crates/workspaces/src/k8s/environment.rs`, `crates/workspaces/src/k8s/policies.rs`, `crates/workspaces/src/k8s/tests/mod.rs`

**Interfaces:**
- Consumes: Task 2's image string.
- Produces:
  - `k8s::proxy_pod_name(service: &str) -> String` → `intercept-{service}`
  - `k8s::target_service_name(ws_id: &str) -> String` → `intercept-target-{ws_id}`
  - `k8s::proxy_selector(service: &str) -> BTreeMap<String, String>` — the labels the ClusterIP switches to
  - `k8s::ProxyRender { pod: Pod, target: CoreService, egress: NetworkPolicy }`
  - `k8s::intercept_render(args: RenderArgs) -> Result<ProxyRender, String>`
  - `k8s::service_clusterip(svc, env_id, owner, owner_ref, intercepted: bool)` — **signature unchanged**, but `intercepted` now switches the selector instead of dropping it.
  - `k8s::intercept_egress(env_ns, ws_ns, ws_id, service, owner, owner_ref)` — one extra `service` parameter, narrowing the policy's own `podSelector` to that proxy pod.

**Steps:**

- [ ] 1. `crates/workspaces/src/k8s/intercept.rs`. Module doc: the objects an in-force intercept needs, why they are where they are (an ownerReference may not cross namespaces, so the workspace-side Service is the Workspace's), and the one fact the design rests on — *NetworkPolicy is evaluated after DNAT, on the backend pod's address, so an egress rule whose peer is `namespaceSelector: ws_ns` AND `podSelector: WORKSPACE_LABEL` admits traffic sent to that Service's ClusterIP.* Then:

```rust
use super::*;

pub fn proxy_pod_name(service: &str) -> String {
    format!("intercept-{service}")
}

/// One Service per WORKSPACE, not per service: a workspace serving two of an environment's
/// services has one object, whose ports are the union both proxies dial.
pub fn target_service_name(ws_id: &str) -> String {
    format!("intercept-target-{ws_id}")
}

/// What the ClusterIP selects while the intercept is in force. `SERVICE_LABEL` carries the
/// service name so a second intercept in the same namespace cannot match this pod.
pub fn proxy_selector(owner: &str, service: &str) -> BTreeMap<String, String> {
    let mut l = labels(owner, "intercept");
    l.insert(SERVICE_LABEL.to_string(), service.to_string());
    l
}

pub struct RenderArgs<'a> {
    pub svc: &'a model::Service,
    pub ic: &'a crate::crd::Intercept,
    pub env_id: &'a str,
    pub owner: &'a str,
    /// The Environment — owns the proxy Pod and the egress policy.
    pub env_ref: &'a OwnerReference,
    pub ws_id: &'a str,
    pub ws_ns: &'a str,
    /// The Workspace — owns the target Service, since an ownerReference may not cross namespaces.
    pub ws_ref: &'a OwnerReference,
    /// The workspace-side ports of EVERY service this workspace serves in this environment
    /// (`intercepted_ports`), which is also what the ingress policy is scoped to.
    pub ws_ports: &'a [u16],
    pub image: &'a str,
    pub runtime_class: Option<&'a str>,
}

pub struct ProxyRender {
    pub pod: Pod,
    pub target: CoreService,
    pub egress: NetworkPolicy,
}

pub fn intercept_render(a: RenderArgs<'_>) -> Result<ProxyRender, String> {
    if a.image.is_empty() {
        return Err("no intercept proxy image is configured on this region's agent".into());
    }
    Ok(ProxyRender { pod: proxy_pod(&a), target: target_service(&a), egress: intercept_egress(&crate::crd::env_namespace(a.env_id), a.ws_ns, a.ws_id, &a.svc.name, a.owner, a.env_ref) })
}
```

- [ ] 2. `proxy_pod` in the same file. Points to get exactly right:
  - name `proxy_pod_name(&a.svc.name)`, namespace `crd::env_namespace(a.env_id)`, `meta(..., a.owner, "intercept", a.env_ref)`, then insert `SERVICE_LABEL` into the labels so `proxy_selector` matches.
  - args: `--target {target_service_name(ws)}.{ws_ns}.svc.cluster.local`, then one `--forward {p}:{ic.workspace_port(p)}` per `a.svc.ports` **in declared order**.
  - `restart_policy: Some("Always")`, `runtime_class_name: a.runtime_class.map(str::to_string)` — the proxy runs under gvisor exactly when the environment's own services do; a pod in the data path must not be the one thing in the namespace outside the sandbox.
  - `security_context: Some(hardened())` plus `run_as_user: Some(1000)`, `run_as_non_root: Some(true)`, `read_only_root_filesystem: Some(true)`. Make `hardened()` `pub(crate)` if it is not already.
  - resources: requests `cpu: 10m`, `memory: 32Mi`; limits `cpu: 200m`, `memory: 128Mi`, with the comment *"it copies bytes; counted against the namespace ResourceQuota like everything else, with no `Quota` dimension of its own."*
  - `ports`: one `ContainerPort` per listen port, named `p{port}`.
  - `readiness_probe`: `tcpSocket` on the FIRST listen port, `period_seconds: 2`, `failure_threshold: 3`. Comment: *"Readiness is the listener being up — Kubernetes' own check, no health endpoint and no probe port. It is also what keeps the Service's endpoint from appearing before the proxy can accept."*
  - `automount_service_account_token: Some(false)` — it makes no API call.

- [ ] 3. `target_service` in the same file: a `CoreService` named `target_service_name(a.ws_id)` in `a.ws_ns`, owned by `a.ws_ref`, `selector` = `{ WORKSPACE_LABEL: ws_id }`, `ports` = one per `a.ws_ports`, each `name: p{port}`, `port`/`target_port` the same. Comment why `ws_ports` is the union and not this service's ports alone.

- [ ] 4. `crates/workspaces/src/k8s/policies.rs`: `intercept_egress` takes a `service: &str` and its `podSelector` becomes `proxy_selector(owner, service)` instead of `{}`. Rewrite the doc: *"Strictly tighter than before: only the proxy may reach the workspace, where every pod in the environment could dial it directly."* Its name stays `intercept_policy_name(ws_id)` — **no**: it must now be per service on the environment side, since one policy per workspace with a per-service podSelector cannot express two proxies. Add `intercept_egress_name(ws_id, service) -> format!("intercept-{ws_id}-{service}")` for the ENVIRONMENT side only; `intercept_policy_name(ws_id)` keeps its name and meaning for the WORKSPACE side (one ingress per workspace, the union of ports), so the two halves no longer share a name. State this split in the module doc — it is the one place a reader will expect symmetry and not find it.

- [ ] 5. `crates/workspaces/src/k8s/environment.rs`: `service_clusterip`'s `intercepted` branch now sets the selector to `proxy_selector(owner, &svc.name)` instead of `None`. Rewrite its doc paragraph: the Service KEEPS a selector, the endpoints are Kubernetes' own, and the ClusterIP, the DNS name and the ports callers dial are untouched. Leave `intercept_slice` in place for now — Task 5 deletes it, and deleting it here breaks the build of a caller this task does not touch.

- [ ] 6. `crates/workspaces/src/k8s/mod.rs`: `mod intercept;` + `pub use intercept::*;`, and a line in the module map comment (`- `intercept`: the proxy pod, the workspace-side target Service and the proxy's egress grant`).

- [ ] 7. `crates/workspaces/src/k8s/tests/intercept.rs`, registered in `tests/mod.rs`. Copy the fixture style of `tests/environment.rs`:
```rust
//! What an in-force intercept renders. Nothing here touches a cluster.

use super::*;

fn args<'a>(svc: &'a model::Service, ic: &'a crd::Intercept, env_ref: &'a OwnerReference, ws_ref: &'a OwnerReference, ports: &'a [u16]) -> k8s::RenderArgs<'a> {
    k8s::RenderArgs {
        svc, ic, env_id: "1", owner: "alice", env_ref,
        ws_id: "ws-1", ws_ns: "ws-alice", ws_ref, ws_ports: ports,
        image: "ghcr.io/kloudlite/kloudlite-intercept-proxy:test", runtime_class: Some("gvisor"),
    }
}

#[test]
fn the_proxy_forwards_one_port_per_declared_service_port_with_the_remap() {
    let svc = service("api", &[8080, 9229]);
    let ic = intercept("api", "ws-1", &[(8080, 3000)]);
    let r = k8s::intercept_render(args(&svc, &ic, &env_ref(), &ws_ref(), &[3000, 9229])).unwrap();
    let c = &r.pod.spec.as_ref().unwrap().containers[0];
    assert_eq!(
        c.args.as_ref().unwrap(),
        &vec![
            "--target".to_string(), "intercept-target-ws-1.ws-alice.svc.cluster.local".to_string(),
            "--forward".to_string(), "8080:3000".to_string(),
            // Unmapped ports forward straight through — `workspace_port` is identity for them.
            "--forward".to_string(), "9229:9229".to_string(),
        ]
    );
    assert_eq!(r.pod.spec.as_ref().unwrap().runtime_class_name.as_deref(), Some("gvisor"), "a data-path pod must be sandboxed exactly when its neighbours are");
    let sc = c.security_context.as_ref().unwrap();
    assert_eq!(sc.run_as_user, Some(1000));
    assert_eq!(sc.read_only_root_filesystem, Some(true));
}

#[test]
fn the_clusterip_keeps_a_selector_and_it_names_the_proxy() {
    let svc = service("api", &[8080]);
    let cs = k8s::service_clusterip(&svc, "1", "alice", &env_ref(), true).unwrap();
    let sel = cs.spec.as_ref().unwrap().selector.as_ref().expect("an intercepted Service must keep a selector — that is the whole fix");
    assert_eq!(sel.get(k8s::KIND_LABEL).map(String::as_str), Some("intercept"));
    assert_eq!(sel.get(k8s::SERVICE_LABEL).map(String::as_str), Some("api"));
}

#[test]
fn the_target_service_publishes_the_workspace_side_union() {
    let svc = service("api", &[8080]);
    let ic = intercept("api", "ws-1", &[(8080, 3000)]);
    let r = k8s::intercept_render(args(&svc, &ic, &env_ref(), &ws_ref(), &[3000, 5432])).unwrap();
    assert_eq!(r.target.metadata.namespace.as_deref(), Some("ws-alice"));
    assert_eq!(r.target.metadata.owner_references.as_ref().unwrap()[0].kind, "Workspace", "an ownerReference may not cross namespaces");
    let ports = r.target.spec.as_ref().unwrap().ports.as_ref().unwrap();
    assert_eq!(ports.iter().map(|p| (p.name.clone().unwrap(), p.port)).collect::<Vec<_>>(), vec![("p3000".into(), 3000), ("p5432".into(), 5432)]);
    assert_eq!(r.target.spec.as_ref().unwrap().selector.as_ref().unwrap().get(k8s::WORKSPACE_LABEL).map(String::as_str), Some("ws-1"));
}

#[test]
fn the_egress_peer_is_the_workspace_pod_and_the_subject_is_the_proxy_alone() {
    let svc = service("api", &[8080]);
    let ic = intercept("api", "ws-1", &[(8080, 3000)]);
    let r = k8s::intercept_render(args(&svc, &ic, &env_ref(), &ws_ref(), &[3000])).unwrap();
    let spec = serde_json::to_value(r.egress.spec.unwrap()).unwrap();
    assert_eq!(spec["podSelector"]["matchLabels"][k8s::SERVICE_LABEL], "api", "every pod in the environment must NOT be the subject any more");
    let to = &spec["egress"][0]["to"];
    assert_eq!(to.as_array().unwrap().len(), 1, "namespace and pod selector must AND, in one element");
    assert_eq!(to[0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"], "ws-alice");
    assert_eq!(to[0]["podSelector"]["matchLabels"][k8s::WORKSPACE_LABEL], "ws-1");
}

#[test]
fn an_unset_image_is_an_error_not_a_pod_that_cannot_pull() {
    let svc = service("api", &[8080]);
    let ic = intercept("api", "ws-1", &[]);
    let mut a = args(&svc, &ic, &env_ref(), &ws_ref(), &[8080]);
    a.image = "";
    assert!(k8s::intercept_render(a).is_err());
}
```
  Write the `service`, `intercept`, `env_ref`, `ws_ref` helpers beside them or reuse `tests/environment.rs`'s if they already exist — check first, do not duplicate.

- [ ] 8. `cargo test -p kloudlite-workspaces` and `cargo clippy --workspace --all-targets -- -D warnings`.

- [ ] 9. `git log -3`, stage, commit: `Render the intercept proxy, its target service and its grant`.

---

### Task 4: `Environment.status.services[].proxy`

**Files:**
- Modify: `crates/workspaces/src/crd/environment.rs`, `deploy/k3s/crds.yaml` (regenerated), `bins/agent/src/controller/environment/run.rs` (the `ServiceStatus` constructor in its tests), `web/apps/web/src/lib/api/environments.ts` (+ the detail view that renders `intercepted_by`)

**Interfaces:**
- Produces: `crd::ServiceStatus::proxy: Option<String>` — `"starting" | "ready" | "failed"`, absent when the service is not intercepted.

**Steps:**

- [ ] 1. `crates/workspaces/src/crd/environment.rs`, after `intercepted_by`:
```rust
    /// The proxy pod backing this intercept: `starting` until it is Ready, `ready` once the
    /// selector points at it, `failed` when the pod is Failed or its container cannot start (an
    /// image pull, most often). Absent when the service is not intercepted.
    ///
    /// A string rather than an enum: it is read by the web and by `kubectl get -o json`, and a
    /// fourth state added later must not make an older object fail to parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
```
  Update the module `//!` line that lists what the web reads.

- [ ] 2. Regenerate: `CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml`.

- [ ] 3. Fix every `ServiceStatus { … }` literal the compiler now rejects (`bins/agent/src/controller/environment/run.rs` has one in its tests; `rg 'ServiceStatus \{'` for the rest) with `proxy: None`.

- [ ] 4. Web: add `proxy?: string` to the service-status type in `web/apps/web/src/lib/api/environments.ts`, and render it beside the existing "intercepted by" line in the environment detail view (`rg -l 'intercepted_by' web/apps/web/src`) as a muted pill: `ready` → nothing extra, `starting` → "proxy starting", `failed` → "proxy failed". The point is that *"my intercept is on but nothing answers"* has an answer on the page rather than in a controller log. Tokens over raw Tailwind colors, copy a sibling pill.

- [ ] 5. Run:
```sh
cargo test -p kloudlite-workspaces
cd web && bun run typecheck && bun run lint
```

- [ ] 6. `git log -3`, stage, commit: `Report the intercept proxy's state per service`.

---

### Task 5: The reconcile — selector to proxy, slice gone

**Files:**
- Modify: `bins/agent/src/controller/environment/run.rs`, `bins/agent/src/controller/environment/intercept.rs`, `bins/agent/src/controller/environment/services.rs`, `crates/workspaces/src/k8s/environment.rs`
- Create: `bins/agent/tests/reconcile/intercept_proxy.rs`
- Modify: `bins/agent/tests/reconcile/main.rs`, `bins/agent/tests/reconcile/filter_foreign_snapshots_against_the_nod.rs` (the shared intercept fixtures live there)

**Interfaces:**
- Consumes: Task 3's `intercept_render`, Task 4's `proxy` field, Task 2's `Ctx::intercept_proxy_image`.
- Produces: `apply_intercept(...) -> Result<Option<String>, ReconcileErr>` (the `proxy` state to record) and `release_intercept(...)` in `environment/intercept.rs`.

**Steps:**

- [ ] 1. In `environment/intercept.rs`, add `apply_intercept`: given the environment, the service, the wish, the `Force` decision's workspace, the namespaces, the owner refs and `ctx`, it
  1. `ensure`s the target Service in the workspace namespace,
  2. `ensure`s the proxy Pod in the environment namespace,
  3. `ensure`s the egress policy (env side) and the ingress policy (workspace side, unchanged, `intercepted_ports` union),
  4. GETs the proxy Pod back and returns `Some("ready")` when `pod_ready(&p).0`, `Some("failed")` when `p.status.phase == "Failed"` or any `containerStatuses[].state.waiting.reason` is `ImagePullBackOff`/`ErrImagePull`/`CreateContainerError` (carry the pod's own message into the condition), else `Some("starting")`.
  A pod already present is NOT recreated on an args change — `ensure`'s apply patches it, and a Pod's `spec.containers[].args` is immutable, so the apply fails. Handle it explicitly: if the existing pod's args differ from the rendered ones, DELETE it, `forget_applied`, and return `Some("starting")`; the next pass creates it. Comment: *"the spec of an intercept is immutable while it runs; a change to ports or workspace is a new pod, and this is where that becomes true."*
  An empty `ctx.intercept_proxy_image` returns `Err`-free `None` with the reason `ProxyImageUnset` — see Task 2 step 7; thread it as an `Intercepting::Off` in `intercept_plan` instead, beside `invalid_port_map`, so the real service stays up and the condition says exactly what to fix.

- [ ] 2. `release_intercept(ws_id, service, env_ns, ws_ns, ctx)`: delete the proxy Pod and its egress policy in the environment namespace, delete the target Service in the workspace namespace **only when no other service is still intercepted by that workspace** (it is per workspace), delete the ingress policy on the same rule, and call `forget_applied` after EVERY delete, for the exact kind/namespace/name. Reuse the existing stale-workspace resolution in `intercept_policies` (cache, GET fallback, an API error keeps the record for a retry) rather than writing a second one — fold `intercept_policies`' delete loop and this into one function if that reads better; it is the same sweep.

- [ ] 3. Rewrite `apply_services` (phase 4) in `run.rs`:
  - `intercepted` is computed exactly as today (`Force` → true, `Keep` → `was_intercepted(prev)`, else false), and the StatefulSet still goes to `replicas: 0` when intercepted.
  - **Order**: for a `Force`, call `apply_intercept` FIRST. Only when it answers `"ready"` does `service_clusterip(..., intercepted = true)` get written and the StatefulSet go to 0; while it is `"starting"` or `"failed"`, the ClusterIP keeps the real selector and the StatefulSet keeps its replicas. Comment the inversion: *"the old spec wrote the slice before the selector went; here the proxy must be Ready before the selector moves — the same principle, that the service is never without a ready endpoint."*
  - `Keep` with `intercepted` true: write nothing about the proxy and leave the selector where it is.
  - Not intercepted, and `decided.is_some() || was_intercepted(prev)`: put the selector back and the replicas back FIRST, then `release_intercept`.
  - Delete the `drop_abandoned_endpoints` call and the whole function in `services.rs` — with the selector kept, Kubernetes abandons nothing. Delete the `Endpoints` import it needed.
  - **Migration**: in the same not-intercepted branch AND on the first pass that renders a proxy, delete the legacy slice `{service}-intercept` (`delete_ignoring_404` + `forget_applied("EndpointSlice", …)`). Comment:
```rust
// The legacy hand-written slice from the endpoint-rewriting mechanism. An intercept in force
// under the old shape is converted by this pass with no downtime and no flag: the proxy comes
// up, the selector goes back (a selector-less Service gaining one is an ordinary update), and
// the slice goes. Mixed builds are safe both ways — an old agent rewrites both to its own
// shape, a new agent deletes what it finds. Delete this, `k8s::intercept_slice` and the
// endpointslices RBAC row one release after the fleet is fully on this build.
```
  - Phase 5a: record the `proxy` value `apply_intercept` returned into the service's `ServiceStatus`, `None` when not intercepted.

- [ ] 4. Delete `k8s::intercept_slice` and its tests **only if nothing else calls it** (`rg intercept_slice`) — the migration delete above is by NAME, not by rendering one, so it should be dead. If anything still calls it, leave it and say so in the commit message.

- [ ] 5. `bins/agent/tests/reconcile/intercept_proxy.rs`, registered in `main.rs` next to the others. Reuse `intercept_env`, `one_intercept`, `intercept_ctx`, `intercept_routes` from `filter_foreign_snapshots_against_the_nod.rs` — extend the shared route helper there with the new paths rather than writing a second fixture:
```rust
pub(crate) const PROXY_POD: &str = "/api/v1/namespaces/env-1/pods/intercept-web";
pub(crate) const TARGET_SVC: &str = "/api/v1/namespaces/ws-alice/services/intercept-target-ws-1";
pub(crate) const WEB_SVC: &str = "/api/v1/namespaces/env-1/services/web";
pub(crate) const ENV_EGRESS: &str = "/apis/networking.k8s.io/v1/namespaces/env-1/networkpolicies/intercept-ws-1-web";
```
  Four tests, each asserting on what the Recorder saw:
```rust
#[tokio::test]
async fn in_force_writes_the_target_and_the_proxy_before_the_selector_moves() {
    // The proxy GET answers Ready, so this pass completes the switch.
    // Assert ORDER, not just presence: target Service, then proxy Pod, then the ClusterIP whose
    // selector names the proxy, then the StatefulSet at 0.
    …
    let order = rec.paths_written();
    assert!(order.iter().position(|p| p == TARGET_SVC) < order.iter().position(|p| p == PROXY_POD));
    assert!(order.iter().position(|p| p == PROXY_POD) < order.iter().position(|p| p == WEB_SVC));
    let svc = rec.last_body(WEB_SVC).unwrap();
    assert_eq!(svc["spec"]["selector"]["kloudlite.io/kind"], "intercept");
    assert_eq!(rec.last_body(WEB_STS).unwrap()["spec"]["replicas"], 0);
}

#[tokio::test]
async fn a_proxy_that_is_not_ready_leaves_the_real_service_serving() {
    // The proxy GET answers Ready=False. The selector must still name the StatefulSet and the
    // replicas must not be 0 — a half-done switch is a service with no endpoints at all.
    …
    assert_eq!(rec.last_body(WEB_SVC).unwrap()["spec"]["selector"]["kloudlite.io/service"], "web");
    assert_ne!(rec.last_body(WEB_STS).unwrap()["spec"]["replicas"], 0);
    assert_eq!(status_proxy(&rec, "web"), Some("starting"));
}

#[tokio::test]
async fn a_release_restores_the_service_before_it_deletes_the_proxy() {
    // Previously intercepted, the wish gone. Selector and replicas back FIRST, then the deletes.
    …
    let order = rec.paths();
    assert!(order.iter().position(|p| p == WEB_SVC) < order.iter().position(|p| p == PROXY_POD));
    assert!(rec.deleted(PROXY_POD) && rec.deleted(TARGET_SVC) && rec.deleted(ENV_EGRESS) && rec.deleted(WS_POLICY));
    // The apply-hash cache must forget every one of them, or the next intercept renders nothing.
    for name in ["intercept-web", "intercept-target-ws-1", "intercept-ws-1-web", "intercept-ws-1"] {
        assert!(!ctx.applied_contains(name), "{name} was deleted without forget_applied");
    }
}

#[tokio::test]
async fn an_intercept_in_force_under_the_old_mechanism_is_converted_in_place() {
    // A Service with NO selector and a `web-intercept` slice, as the previous build left it.
    // One pass: the proxy is rendered, the selector comes back naming it, and the slice is deleted.
    …
    assert!(rec.deleted(WEB_SLICE));
    assert_eq!(rec.last_body(WEB_SVC).unwrap()["spec"]["selector"]["kloudlite.io/kind"], "intercept");
}
```
  `paths_written`, `last_body`, `deleted`, `applied_contains` — use whatever the existing `Recorder` actually exposes; read `crates/workspaces/src/kube_test.rs` first and mirror an existing assertion rather than inventing an API. Delete the old slice-shaped tests in `filter_foreign_snapshots_against_the_nod.rs` that no longer describe the mechanism, keeping every one that is about the DECISION (`Keep`, grace, `WorkspaceGone`) — those are untouched by this plan.

- [ ] 6. Run:
```sh
cargo test -p kloudlite-agent --test reconcile
cargo test
cargo clippy --workspace --all-targets -- -D warnings
```

- [ ] 7. `git log -3`, stage, commit: `Back an intercepted service with a proxy pod instead of a slice`.

---

### Task 6: `/v1` refusals

**Files:**
- Modify: `crates/workspaces/src/api/environments.rs`, `crates/workspaces/tests/api_environments.rs` (or wherever `validate_intercept`'s tests live — `rg 'does not declare port'` to find them)

**Steps:**

- [ ] 1. In `validate_intercept`, after the existing per-port checks, refuse a UDP service port with 422 naming the port:
```rust
// The forwarder is TCP. A UDP port rendered as an intercept would scale the real service to 0
// and drop half a protocol on the floor, which is worse than refusing. Nothing declares UDP
// today (`service_clusterip` hard-codes TCP), so this is a guard for when `model::Service`
// grows a protocol, not a migration.
if svc.protocol_of(p.service).is_some_and(|proto| !proto.eq_ignore_ascii_case("TCP")) {
    return Err((
        StatusCode::UNPROCESSABLE_ENTITY,
        format!("port {} is UDP, and an intercept forwards TCP only", p.service),
    ).into_response());
}
```
  `model::Service` has no protocol today. **Do not add one.** Instead write the guard against whatever field exists when this runs: if `model::Service` still has no protocol, add a single `// ponytail:` line where the check would go, naming the upgrade path, and cover the 422 in the SLO id as "skipped until `model::Service` carries a protocol". Decide from the code, and say which you did in the commit message.

- [ ] 2. The 7788 refusal (`ide_port_collision`) is unchanged. Add one test asserting it still 422s through this path, so the rewrite cannot quietly drop it:
```rust
#[tokio::test]
async fn the_tool_server_port_is_still_refused_after_the_proxy_rewrite() { … assert 422 and that the message names 7788 … }
```

- [ ] 3. `cargo test -p kloudlite-workspaces`, then `git log -3`, stage, commit: `Refuse a UDP intercept and keep the tool-server refusal`.

---

### Task 7: The SLO journey

**Files:**
- Modify: `bins/slo/src/stages/environment.rs`, `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`

**Interfaces:**
- Produces nine ids in the Hourly suite, stage `6 · Environment`. `deploy/slo.md` is held equal to the catalogue by an existing test — edit both or that test fails.

**The rule for every step: judge on OUTPUT.** A step that asserts an object's state and not an answer is not a probe of this mechanism — the whole bug being fixed is a case where every object was perfect and the packets were dropped. Every id below ends in a dial whose BYTES are checked.

**Steps:**

- [ ] 1. Catalogue rows (`crates/workspaces/src/slo/catalogue.rs`), beside the existing three, and the same rows in `deploy/slo.md`'s table:

| id | SLI | target |
|---|---|---|
| `env.intercept.proxy.up` | The proxy pod for an intercepted service is Ready and the service's endpoints name it | 95 % ≤ 90000 ms |
| `env.intercept.delivered` | A request from an environment pod to the intercepted service is answered by the workspace | 95 % ≤ 120000 ms |
| `env.intercept.remap` | The intercepted service answers on its own port while the workspace listens on another | 99.9 % |
| `env.intercept.peer` | A second space following the same environment reaches the intercepted service | 95 % ≤ 120000 ms |
| `env.intercept.bench` | The probe owner's bench reaches the intercepted service | 95 % ≤ 120000 ms |
| `env.intercept.proxy.restart` | Killing the intercepting workspace's pod does not interrupt delivery beyond the pod's own restart, and no proxy is recreated | 95 % ≤ 180000 ms |
| `env.intercept.release` | Releasing the intercept brings the real service back, removes the proxy and leaves no grant behind | 95 % ≤ 180000 ms |
| `env.intercept.udp.refused` | An intercept of a UDP port is refused | 99.9 % |
| `env.intercept.tools.refused` | An intercept mapping onto the tool server's port is refused | 99.9 % |

  Keep `env.intercept`, `env.intercept.fallback`, `env.intercept.refused` with their ids, ceilings and meanings — the mechanism changed, the promise did not. Extend `INTERCEPT_IDS` to all of them (it is the skip list a fast run uses; a missing id reads as passed).

- [ ] 2. `env.intercept.proxy.up` — after the POST that sets the intercept, poll `GET /v1/environments/{id}` until that service's `proxy == "ready"`, and then dial once from a sibling pod and require the MARKER. State-only would pass on the old mechanism.

- [ ] 3. `env.intercept.delivered` and `env.intercept.remap` — reuse `dial`/`workspace_dial`/`answers` exactly as `env.intercept` does today (the environment's own `SERVICE` pod, `TARGET:8080`, body `slo-intercept`). `remap` additionally asserts the workspace's listener is on `WS_PORT` (3000) and that dialling `TARGET:3000` from the environment does NOT answer — the remap is real, not two ports that happen to agree.

- [ ] 4. `env.intercept.peer` — **the bug, as a probe.** The suite already owns a second probe owner per suite pair; use it:
  - `PUT /v1/me/environments/{team}` as the SECOND owner naming the SAME environment (the environment is the team's, so the second owner's space can follow it — if the suite's second owner is not in that team, add them through the directory the same way stage 6 already does for `env.space.bench`; if that is not available, create the second owner's space against the team environment and skip with the reason rather than passing).
  - Create a `run-{id}` workspace for that second owner, wait Ready.
  - `kubectl exec` into THAT workspace's pod and dial `{TARGET}.{env_ns}.svc.cluster.local:8080`, requiring `MARKER`.
  Comment in the source: *"This would have failed every hour since spaces shipped: the follower's packets were DNAT'd to a pod in another owner's namespace, whose ingress policy admitted only the environment's."*
- [ ] 5. `env.intercept.bench` — the probe bench (stage 6 already stands one up for `env.space.bench`) dials the same address and requires `MARKER`. Read through the bench's own exec path that `env.space.bench` uses; do not invent a second one.

- [ ] 6. `env.intercept.proxy.restart` — record the proxy pod's `metadata.uid` (through the env document's `proxy` state plus a `kubectl get pod`), then `kubectl delete pod` the INTERCEPTING WORKSPACE's pod. Then:
  - poll the dial from the environment until `MARKER` comes back (the workspace pod restarts; the target Service absorbs the new IP),
  - assert the proxy pod's uid is UNCHANGED.
  That second assertion is option (b)'s whole claim and the only way to catch a regression to baked-in addressing.

- [ ] 7. `env.intercept.release` — `DELETE /v1/environments/{id}/intercepts/{service}`, then all four in one step:
  - `answers(..., &service_dial(), "PONG", …)` — the real service answers, from inside the environment;
  - the real StatefulSet is back at 1 ready replica (`sts_ready`);
  - `kubectl get pod -n env-{id} -l kloudlite.io/kind=intercept` is EMPTY;
  - `kubectl get networkpolicy -n env-{id} -l kloudlite.io/kind=policy` names no `intercept-*`, and the same in the workspace namespace — listed BY LABEL, never by guessing a name, so a grant left under a name this probe does not know is still caught.

- [ ] 8. `env.intercept.udp.refused` — POST an intercept whose port the environment declares as UDP and require 422 naming the port. If Task 6 concluded `model::Service` has no protocol yet, `c.skip("env.intercept.udp.refused", "model::Service carries no protocol yet")` with that exact sentence — a skip is visible in the report; a silent pass is not.

- [ ] 9. `env.intercept.tools.refused` — POST `{"ports": [{"service": TARGET_PORT, "workspace": 7788}]}` and require 422 whose body names `7788`. Fold it beside the existing `refused` step's helpers (`expect_refusal`), not a new mechanism.

- [ ] 10. **Teardown.** Extend the stage's teardown so it can never leave a proxy behind:
  - `DELETE` every intercept the stage set, before deleting the environment (a release is what removes the proxy; an environment delete collects it through the ownerRef, but a teardown that raced the delete has left one running on the fleet before).
  - delete the second owner's space and workspace by the `run-{id}` prefix rule, exactly as the rest of the suite does — nothing is deleted by a name that is not prefixed, so a crashed run is swept by the next one and a run can never delete another's live objects.
  - after the environment delete, assert (do not just hope) that no pod labelled `kloudlite.io/kind=intercept` remains in the run's namespaces; report it as a teardown failure if one does.

- [ ] 11. Run:
```sh
cargo test -p kloudlite-workspaces --test slo_catalogue   # deploy/slo.md ↔ catalogue equality
cargo test -p kloudlite-slo
cargo clippy --workspace --all-targets -- -D warnings
```

- [ ] 12. `git log -3`, stage, commit: `Probe the intercept proxy end to end from every vantage point`.

---

### Task 8: RBAC

**Files:**
- Modify: `deploy/k3s/agent-rbac.yaml`

**Steps:**

- [ ] 1. **The table is the role.** Update these rows, in the table AND in the rules:
  - `pods`: add `create,delete` — *"the intercept proxy pod in an environment namespace; `delete` recreates one whose forwards changed and releases one on intercept release."* (`get,list,watch` already there.)
  - `services`: the `create,patch,delete` row gains *"and the `intercept-target-{ws}` ClusterIP in the WORKSPACE namespace, which the proxy dials"*. No new verb.
  - `networkpolicies`: no new verb; note the env-side name is now per service (`intercept-{ws}-{service}`).
  - `endpointslices`: keep `create,patch` for one release and add `delete` if it is not there (the migration deletes the legacy slice). Add the line: *"Retired with the endpoint-rewriting mechanism — delete this row and the rule one release after the fleet is fully on the proxy build."*

- [ ] 2. Confirm the rules block matches the table exactly; a verb in one and not the other is the bug this file's header exists to prevent.

- [ ] 3. `git log -3`, stage, commit: `Grant the agent the proxy pod and its target service`.

---

### Task 9: CLAUDE.md and docs

**Files:**
- Modify: `CLAUDE.md`

**Steps:**

- [ ] 1. Rewrite the intercept paragraph in "Workspaces and environments". What must go, because it is now false: *"Traffic moves by ENDPOINTS, never DNS and never a proxy"*, the `EndpointSlice` sentence, the selector-less Service, and the whole "Kubernetes does not clean up after a Service that loses its selector" passage (`drop_abandoned_endpoints` is gone). What must be there:
  - the real service's StatefulSet is still scaled to 0, for the unchanged queue-consumer reason;
  - the ClusterIP **keeps its selector** and now names a proxy pod `intercept-{service}` in the environment's own namespace, one per intercepted service, owned by the Environment, running `kloudlite-intercept-proxy` — a byte-for-byte TCP forwarder, protocol-blind, no configuration a person ever sees;
  - the proxy dials `intercept-target-{ws}.{ws_ns}.svc.cluster.local`, a ClusterIP in the workspace namespace owned by the WORKSPACE (an ownerReference may not cross namespaces), so a workspace pod restart moves nothing of ours;
  - **why**: the endpoints are then in the environment's namespace, which every following space already reaches through its own `space_egress` grant — the cross-space bug is fixed by construction, with no grant per follower;
  - the ports are still remapped and matched by the port name `p{port}`, 7788 is still refused twice, and the grace, the release and the wish are unchanged;
  - `status.services[].proxy` is what the web shows beside `intercepted_by`.
  Keep the paragraph's density; it is a paragraph, not a section.

- [ ] 2. Add `kloudlite-intercept-proxy` to the binaries list in "Workspace layout" (`bins/{server,api,worker,agent,gateway,intercept-proxy,kl-connect}` — say it is rendered into tenant namespaces by the agent and never deployed as a workload of ours), and to the image list in "Deploying".

- [ ] 3. `git log -3`, stage `CLAUDE.md`, commit: `Document the intercept proxy`.

---

## Ship

Per `CLAUDE.md` "Dev loop" and "Fixed means verified on fleet": edit on the laptop, push to `platform`, pull in the dev pod, build and test there, roll, and only then say it works. The fleet evidence this change needs, in order:

1. `env.intercept` and `env.intercept.delivered` green on the carrying build.
2. `env.intercept.peer` green — the id that did not exist because the bug did.
3. `env.intercept.proxy.restart` green with the proxy uid unchanged.
4. One converted intercept observed going from the old slice shape to the proxy with no gap in `env.intercept`.

Until those four, the change is "shipped, unverified".
