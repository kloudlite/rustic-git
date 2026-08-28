# Workspace SSH Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `kl ws ssh gh` opens an ssh session into a workspace pod's `sshd` through a Cloudflare-fronted WebSocket gateway in the region, with the platform authorizing each connection and the user's own key authenticating the login.

**Architecture:** The api mints a 60 s single-use session JWT (`typ: ssh-session`); the gateway (`bins/gateway`, k3s) verifies it with the shared JWT secret, resolves the pod IP, dials `pod:22`, and pumps WebSocket ↔ TCP; it serves TLS on the nodes' public interface with a Cloudflare Origin CA certificate, reachable only from Cloudflare's ranges (node firewall), published as `ws-<region>.khost.dev` behind the Cloudflare proxy; the CLI (`bins/kl`) is ssh's `ProxyCommand`. The agent gives every default-image pod an `sshd` with a per-workspace host key Secret and the owner's registered keys.

**Tech Stack:** Rust (axum 0.8 WebSocket via `axum` `ws` feature, `tokio-tungstenite` in the CLI, `clap`, `jsonwebtoken` 11, `kube`), `ssh-keygen` from `openssh-client` in the agent image, Cloudflare proxy + Origin CA certificate, Next.js web.

**Spec:** `docs/superpowers/specs/2026-08-28-workspace-ssh-design.md`

## Global Constraints

- Session token: JWT `typ: "ssh-session"`, claims `{ sub: <owner username>, ws: <workspace id>, region, jti, iat, exp }`, `exp - iat = 60`. Single use per gateway replica (in-memory `jti` set with expiry). Signed with the same `RUSTIC_GIT_JWT_SECRET` the api uses; the gateway gets it from Secret `rustic-git-jwt` in `kube-system` (k3s).
- CLI token: JWT `typ: "cli"`, 30 days, `jti`; revocable — the directory stores `Credential { kind: CliToken, id: jti }` and `caller()` refuses a `cli` token whose `jti` is not present (revoke = delete the row). `caller()` accepts `typ` `session` or `cli`.
- Gateway route `GET /tunnel/{ws-id}` with `Authorization: Bearer <session token>`; the token's `ws` must equal the path; 401 bad/expired/used token, 404 no such workspace, 409 not ready, 502 dial failed; otherwise `101` and binary frames. Idle timeout 30 min; 64 KiB max frame; limits: 10 concurrent per workspace, 100 per owner (per replica).
- Pod: default image only; command `["/nix/profile/current/bin/sshd", "-D", "-e", "-f", "/etc/ssh/sshd_config"]`; `/etc/ssh` from Secret `ws-ssh-{id}` (`ssh_host_ed25519_key` 0400, `ssh_host_ed25519_key.pub`, `sshd_config`), `/root/.ssh/authorized_keys` from Secret `user-key` key `authorized_keys` (0600). `sshd_config` exactly as in the spec. NetworkPolicy: ingress 22 only from pods labelled `app=rustic-git-gateway` in namespace `kube-system`.
- `status.sshHostKey` on the Workspace = the public host key line.
- Registered SSH keys store their material from now on (`Credential.material` for `SshKey`); `authorized_keys` = all the owner's SshKey material lines, rewritten on every add/remove and on `install_user_key`.
- `/v1` is public on `dev.kloudlite.io` via a path rule to `rustic-git-api`, rate-limited like the app.
- Hostnames: `ws-<region-id>.khost.dev` (e.g. `ws-centralindia-k3s.khost.dev`).
- The only inbound on a node is 443 from Cloudflare's published ranges (node firewall); nothing else changes.
- Tokens never logged. CLI config `~/.config/kl/config.json` mode 0600.
- Comments explain WHY; `// ponytail:` for deliberate ceilings; commit subjects imperative sentence case, no tool attribution.

---

## File map

| File | Responsibility |
|---|---|
| `crates/core/src/jwt.rs` | `mint_typed(claims, ttl)` / `verify_typed(token, typ)`; `SshSessionClaims`, `CliClaims`. |
| `crates/pulls/src/directory/mod.rs` | `CredentialKind::CliToken`; `material` kept for `SshKey`. |
| `crates/api/src/credentials.rs` | store SSH key material; `POST /v1/cli/code`, `POST /v1/cli/approve`, `GET /v1/cli/token`, `GET/DELETE /v1/cli/tokens`. |
| `crates/workspaces/src/api.rs` | `caller()` accepts `cli`; `authorized_keys` into `user-key`; `POST /v1/workspaces/{id}/ssh-session`; `ws_doc.ssh`. |
| `crates/workspaces/src/crd.rs` | `status.sshHostKey`. |
| `crates/workspaces/src/k8s.rs` | `ws_ssh_secret_name`, `sshd_config()`, pod command/mounts, `allow_gateway_ingress` policy, `user_key_secret` with `authorized_keys`. |
| `bins/agent/src/sshkeys.rs` (new) | host key generation via `ssh-keygen`, Secret ensure, `status.sshHostKey`. |
| `bins/gateway/` (new) | the gateway binary + `deploy/k3s/gateway.yaml`. |
| `bins/kl/` (new) | the CLI; `.github/workflows/kl.yml`; `web/apps/web/public/install.sh`. |
| web | `/cli/authorize` page, Settings → CLI tokens, workspace row ssh snippet, key "re-add" badge. |
| `Dockerfile` | agent stage gains `openssh-client`; `gateway` stage; `kl` built by its own workflow. |

---

### Task 1: Typed JWTs

**Files:**
- Modify: `crates/core/src/jwt.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
  pub struct SshSessionClaims { pub sub: String, pub ws: String, pub region: String, pub jti: String, pub iat: u64, pub exp: u64, pub typ: String }
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
  pub struct CliClaims { pub sub: String, pub name: String, pub username: Option<String>, pub jti: String, pub iat: u64, pub exp: u64, pub typ: String }
  impl Jwt {
      pub fn mint_ssh_session(&self, owner: &str, ws: &str, region: &str) -> Result<(String, SshSessionClaims)>;  // 60 s, jti = 16 random bytes hex
      pub fn verify_ssh_session(&self, token: &str) -> Result<SshSessionClaims>;                                    // typ must be "ssh-session"
      pub fn mint_cli(&self, email: &str, name: &str, username: Option<&str>) -> Result<(String, CliClaims)>;         // 30 days, jti
      pub fn verify_any_user(&self, token: &str) -> Result<(Claims, Option<String>)>;  // session OR cli; returns Claims shape + Some(jti) for cli
  }
  pub const SSH_SESSION_TTL_SECS: u64 = 60;
  pub const CLI_TTL_SECS: u64 = 30 * 24 * 60 * 60;
  ```

- [ ] **Step 1: Failing tests** (jwt.rs tests)
```rust
#[test]
fn an_ssh_session_is_sixty_seconds_single_purpose_and_bound_to_one_workspace() {
    let j = Jwt::new("0123456789abcdef0123456789abcdef").unwrap();
    let (tok, c) = j.mint_ssh_session("karthik1729", "ws-1", "centralindia-k3s").unwrap();
    assert_eq!(c.exp - c.iat, SSH_SESSION_TTL_SECS);
    assert_eq!(c.typ, "ssh-session");
    let back = j.verify_ssh_session(&tok).unwrap();
    assert_eq!((back.sub.as_str(), back.ws.as_str(), back.region.as_str()), ("karthik1729", "ws-1", "centralindia-k3s"));
    assert_eq!(back.jti.len(), 32);
    assert!(j.verify(&tok).is_err(), "a session token is not a login");
    let login = j.mint("a@b.c", "A", Some("a")).unwrap();
    assert!(j.verify_ssh_session(&login).is_err(), "a login is not a session token");
}

#[test]
fn a_cli_token_is_a_user_token_with_an_id_and_a_month() {
    let j = Jwt::new("0123456789abcdef0123456789abcdef").unwrap();
    let (tok, c) = j.mint_cli("a@b.c", "A", Some("a")).unwrap();
    assert_eq!(c.exp - c.iat, CLI_TTL_SECS);
    let (claims, jti) = j.verify_any_user(&tok).unwrap();
    assert_eq!(claims.username.as_deref(), Some("a"));
    assert_eq!(jti.as_deref(), Some(c.jti.as_str()));
    let (claims2, jti2) = j.verify_any_user(&j.mint("a@b.c", "A", Some("a")).unwrap()).unwrap();
    assert_eq!(claims2.typ, "session");
    assert!(jti2.is_none());
}
```
- [ ] **Step 2: Run** `cargo test -p rustic-git-core jwt` → compile error.
- [ ] **Step 3: Implement** — one private `fn now()`; `jti` = 16 random bytes (`rand::random::<[u8;16]>()`, hex) ; `verify_typed` helper decoding into a generic `serde_json::Value` first to read `typ` cheaply, then into the concrete struct. `verify_any_user`: try `verify` (session) else decode as `CliClaims` with `typ == "cli"` and map to `Claims { typ: "cli", .. }`.
- [ ] **Step 4: Run** `cargo test -p rustic-git-core && cargo clippy -p rustic-git-core -- -D warnings`.
- [ ] **Step 5: Commit** `Mint single-purpose ssh-session and cli tokens`.

---

### Task 2: SSH key material, CLI tokens and the `/v1/cli` flow (crates/api)

**Files:**
- Modify: `crates/pulls/src/directory/mod.rs` (`CredentialKind::CliToken`; `credentials_for`), `crates/api/src/credentials.rs`, `crates/api/src/lib.rs` (routes)
- Test: existing `credentials` tests in the crate

**Interfaces:**
- Produces:
  - `Credential.material` populated for `SshKey` with the normalized key line (`<type> <base64> <comment>`).
  - `CredentialKind::CliToken` rows: `id = jti`, `name` = the device name the CLI sent, `created_by`, `owner = username`.
  - Routes: `POST /v1/cli/code` (no auth) `{ "device": "karthik-mbp" }` → `{ "code": "ABCD-EFGH", "poll": "<32-hex>", "expires_in": 600 }`; `POST /v1/cli/approve` (session JWT) `{ "code" }` → 204; `GET /v1/cli/token?poll=<id>` → `202` while pending, `200 { "token": "<cli jwt>", "expires_at" }` once (then the poll id is spent), `410` expired/denied; `GET /v1/cli/tokens` (session) → list `{ id, name, created_at, expires_at }`; `DELETE /v1/cli/tokens/{id}` → 204.
  - Pending codes live in an in-memory `Mutex<HashMap<String, Pending>>` on the `Api` state with a 10 min TTL (`// ponytail: per replica; a second api replica will not see this code — pin /v1/cli/* to one replica via session affinity, or move to the directory, when there is a second replica`).
  - `pub async fn authorized_keys_for(db, owner) -> Result<String>`: the owner's `SshKey` materials joined by `\n` (empty material lines skipped).

- [ ] **Step 1: Failing tests** — in `credentials.rs` tests (the module has a mongo-less unit style for parsing; add): `an_ssh_key_keeps_its_material_and_a_gpg_key_still_keeps_its_own`, `authorized_keys_is_one_line_per_key_and_skips_keys_added_before_material_was_kept`, `the_cli_code_flow_hands_out_a_token_exactly_once` (drive the three handlers with a fake state: code → approve with a minted session → poll 200 → poll 410).
- [ ] **Step 2: Run** → failures.
- [ ] **Step 3: Implement.** Material normalization: take the first three whitespace fields of `body.key`. `caller`-style helper for `/v1/cli/tokens` reuses `credential_caller`. Revocation: `DELETE` removes the row; a `cli` JWT whose `jti` row is missing is refused by `credential_caller` (and by Task 3's `caller()`).
- [ ] **Step 4: Run** `cargo test -p rustic-git-api && cargo clippy -p rustic-git-api -- -D warnings`.
- [ ] **Step 5: Commit** `Keep SSH key material and issue revocable CLI tokens`.

---

### Task 3: Workspaces api — cli callers, authorized_keys, ssh-session

**Files:**
- Modify: `crates/workspaces/src/api.rs` (`caller`, `install_user_key`, new `ssh_session` route, `ws_doc`), `crates/workspaces/src/k8s.rs` (`user_key_secret(owner, team, private, authorized_keys: &str)`), `crates/workspaces/src/model.rs` (`Workspace.ssh: Option<SshDoc>`), `bins/api/src/main.rs` (wire the directory for `authorized_keys_for` and CLI-token revocation check — `ApiState` gains `pub cli_tokens: Option<Arc<dyn CliTokenCheck>>` with `async fn is_live(&self, jti) -> bool`, implemented in bins/api against the directory like `DirMembership`)

**Interfaces:**
- Produces:
  - `caller()` accepts session or cli (`jwt.verify_any_user`); a cli token whose `jti` is not live → 401.
  - `POST /v1/workspaces/{id}/ssh-session` → `201 { "token", "gateway": "wss://ws-<region>.khost.dev/tunnel/<id>", "expires_at", "host_key": "<status.sshHostKey>" }`; 404 if the caller may not act on the workspace; 409 `{ "error": "workspace is <state>" }` if not `ready`; 503 if `sshHostKey` is not on status yet.
  - `ws_doc.ssh = { gateway, host_key }` when `sshHostKey` exists (the web's snippet needs no mint).
  - `user_key_secret` writes `authorized_keys` alongside `id_ed25519`; `install_user_key` fetches `authorized_keys_for(owner)` through a new `ApiState.authorized_keys: Option<Arc<dyn AuthorizedKeys>>` (bins/api implements it over the directory). Also called by the credentials routes on add/remove: Task 2's handlers get a hook `on_keys_changed(owner)` that bins/api wires to rewrite every namespace whose `user-key` Secret exists (`ws-{owner}` and `ws-{owner}-{team}` — list namespaces by the owner label).

- [ ] **Step 1: Failing tests** (api.rs tests + the crate's router tests): `a_cli_token_is_a_caller_until_it_is_revoked`, `an_ssh_session_is_minted_only_for_a_ready_workspace_the_caller_may_act_on` (mocked kube: ready ws with `sshHostKey` → 201 with `ws` claim = id, gateway URL as constrained; not-ready → 409; other owner → 404), `the_user_key_secret_carries_authorized_keys`.
- [ ] **Step 2: Run** → failures.
- [ ] **Step 3: Implement.** Gateway URL = `format!("wss://ws-{}.khost.dev/tunnel/{}", region, id)` — a const `GATEWAY_DOMAIN: &str = "khost.dev"` in api.rs with a WHY comment (one domain, DNS per region created by `cloudflare-tunnel.sh`).
- [ ] **Step 4: Run** `cargo test -p rustic-git-workspaces && cargo clippy -p rustic-git-workspaces -p rustic-git-api-bin -- -D warnings` (check the api bin's package name).
- [ ] **Step 5: Commit** `Mint ssh sessions and keep authorized_keys in every workspace namespace`.

---

### Task 4: CRD, agent: host keys, sshd pod, gateway-only ingress

**Files:**
- Modify: `crates/workspaces/src/crd.rs` (`WorkspaceStatus.ssh_host_key: Option<String>`), regenerate `deploy/k3s/crds.yaml`; `crates/workspaces/src/k8s.rs` (`ws_ssh_secret(id, ns, owner, owner_ref, private, public) -> Secret`, `sshd_config() -> &'static str`, `workspace_pod` command + mounts for the default image, `allow_gateway_ingress(ns, owner, owner_ref) -> NetworkPolicy`); `bins/agent/src/sshkeys.rs` (new), `bins/agent/src/controller.rs` (`ensure_ssh` step after `ensure_profile`, before the pod; `sshHostKey` on status), `deploy/k3s/agent-rbac.yaml` (secrets `get/create/patch` in `ws-*` — cluster-wide verbs; a `resourceNames` restriction cannot express "ws-ssh-*"; note why), `Dockerfile` agent stage (`openssh-client` for `ssh-keygen`)
- Test: `crates/workspaces/src/k8s.rs` tests, `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Produces:
  ```rust
  // sshkeys.rs
  pub trait HostKeys: Send + Sync { fn generate(&self) -> Result<(String, String), String>; }   // (private openssh, public line)
  pub struct SshKeygen;   // runs `ssh-keygen -q -t ed25519 -N "" -C ws -f <tmp>` and reads both files
  pub async fn ensure_ssh(w, id, ns, owner_ref, ctx) -> Result<String /* public line */, ReconcileErr>
  ```
  `Ctx.host_keys: Arc<dyn HostKeys>` (test fake returns fixed strings). Secret `ws-ssh-{id}` created once (get → absent → generate → create); the public line is read back from the Secret on later passes.
  Pod (default image): command per constraints; volumes `ws-ssh` (Secret `ws-ssh-{id}`, defaultMode 0o400) at `/etc/ssh` (read-only) and `user-key` subPath `authorized_keys` at `/root/.ssh/authorized_keys` (read-only, mode 0o600 via `items`). Ports `22`.
  NetworkPolicy `allow-gateway-ssh`: ingress port 22 from `namespaceSelector kubernetes.io/metadata.name=kube-system` + `podSelector app=rustic-git-gateway`.

- [ ] **Step 1: Failing tests**: k8s.rs — `the_default_image_runs_sshd_with_its_own_host_key_and_the_owners_keys` (command, both mounts, port 22, read-only, no hostPath), `a_custom_image_keeps_its_entrypoint_and_gets_no_sshd`, `only_the_gateway_may_reach_port_22`; reconcile.rs — `a_workspace_gets_a_host_key_secret_before_its_pod` (fake HostKeys; Secret POST precedes pod POST; status `sshHostKey` set), `an_existing_host_key_is_reused` (GET returns a Secret → no generate).
- [ ] **Step 2: Run** → failures.
- [ ] **Step 3: Implement.** `ensure_ssh` sits after `ensure_profile` (the pod must not start before both). Agent boot: `host_keys: Arc::new(SshKeygen)`. RBAC: `secrets: get, create, patch` cluster-wide (WHY: per-workspace names). Dockerfile agent stage: `openssh-client`.
- [ ] **Step 4: Run** `cargo test -p rustic-git-workspaces -p rustic-git-agent-bin && cargo clippy --workspace -- -D warnings && CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml`.
- [ ] **Step 5: Commit** `Run sshd in every default workspace pod with a per-workspace host key`.

---

### Task 5: The gateway

**Files:**
- Create: `bins/gateway/Cargo.toml` (package `rustic-git-gateway-bin`, bin `rustic-git-gateway`), `bins/gateway/src/main.rs`, `bins/gateway/src/lib.rs` (`tunnel.rs`: auth + pump; `resolve.rs`: pod IP), `bins/gateway/tests/tunnel.rs`
- Modify: root `Cargo.toml` members; `Dockerfile` (a `gateway` stage from the same builder: debian-slim, non-root uid 1001, `ENTRYPOINT ["rustic-git-gateway"]`); `.github/workflows/image.yml` (build/push `ghcr.io/kloudlite/rustic-git-gateway:<sha>`)
- Create: `deploy/k3s/gateway.yaml` (ServiceAccount + ClusterRole `get` workspaces, `get` pods; Deployment 2 replicas in `kube-system`, label `app=rustic-git-gateway`, env `RUSTIC_GIT_JWT_SECRET` from Secret `rustic-git-jwt`, `WS_REGION`; Service `rustic-git-gateway:8080`)

**Interfaces:**
- `axum` with `ws` feature; `GET /tunnel/{ws}`; `GET /healthz`.
- `struct Used { set: Mutex<HashMap<String /*jti*/, u64 /*exp*/>> }` — insert on use, refuse if present, sweep expired on insert.
- `resolve(client, ws_id) -> Result<(SocketAddr, owner), Refusal>`: Workspace (cluster-scoped) must have `status.phase == ready` and `status.podRef = "<ns>/<name>"`; Pod `status.podIP`; port 22.
- Pump: `tokio::select!` over `ws.next()` → write to TCP, and TCP read → `Message::Binary`; close on either EOF; idle timer 30 min reset on any frame.

- [ ] **Step 1: Failing tests** (`bins/gateway/tests/tunnel.rs`, using the crate's `kube_test` mock pattern from `crates/workspaces` for the Workspace/Pod GETs, and a local TCP echo listener): `a_valid_session_is_pumped_to_the_pod_and_spent` (client sends bytes over ws, gets them echoed; a second upgrade with the same token → 401), `a_token_for_another_workspace_is_refused` (401), `an_unready_workspace_is_409`, `an_expired_token_is_401`.
- [ ] **Step 2: Run** → failures.
- [ ] **Step 3: Implement.** Logging: `owner, ws, bytes_in, bytes_out, secs` at close. Concurrency counters per ws and per owner (`Mutex<HashMap<String, usize>>`), decremented on close.
- [ ] **Step 4: Run** `cargo test -p rustic-git-gateway-bin && cargo clippy -p rustic-git-gateway-bin -- -D warnings`; `kubectl apply --dry-run=client -f deploy/k3s/gateway.yaml`.
- [ ] **Step 5: Commit** `Add the workspace SSH gateway`.

---

### Task 6: The gateway on the region's own nodes, behind the Cloudflare proxy

k3s here runs with Traefik and ServiceLB disabled and has no LoadBalancer, so the gateway binds
the nodes' public interface itself; Cloudflare proxies the hostname; the node firewall admits
443 from Cloudflare's ranges only. No tunnel connector.

**Files:**
- Modify: `deploy/k3s/gateway.yaml` (from Task 5: add `hostPort: 443` on the container, `podAntiAffinity` one-per-node, `nodeSelector rustic-git.io/pool=true`, a `gateway-tls` Secret mount at `/etc/gateway/tls` — `tls.crt`/`tls.key` are a Cloudflare **Origin CA** certificate for `ws-*.khost.dev`, created by the operator in the dashboard (SSL/TLS → Origin Server → Create Certificate, 15 years) and installed with `kubectl -n kube-system create secret tls gateway-tls --cert=… --key=…`)
- Modify: `bins/gateway` — serve TLS itself (`axum-server` with `rustls` from the mounted files; `GATEWAY_TLS_DIR` env; plain HTTP on 8080 stays for tests/health), listen `0.0.0.0:443`.
- Modify: `deploy/k3s/harden-node.sh` — `CF_CIDRS` env (the published v4 list): `iifname "$IFACE" tcp dport 443 ip saddr { <cidrs> } accept`; README documents refreshing it.
- DNS (operator, dashboard): `ws-centralindia-k3s.khost.dev` **A** `40.80.82.158` and **A** `20.219.22.61`, both **proxied**; SSL/TLS mode **Full (strict)** for `khost.dev`.
- Cloudflare rate limit (needs WAF: Edit): `/tunnel/*` 30 req/10 s per IP.

- [ ] **Step 1** gateway TLS + hostPort yaml; `kubectl apply --dry-run=client`.
- [ ] **Step 2** harden-node.sh CF allow; re-run on session-0 and env-0 with `CF_CIDRS`.
- [ ] **Step 3: Verify** — `curl -s -o /dev/null -w '%{http_code}' https://ws-centralindia-k3s.khost.dev/healthz` → 200 via Cloudflare; direct `curl --resolve ws-centralindia-k3s.khost.dev:443:40.80.82.158 …` from outside Cloudflare → timeout (firewall); `/tunnel/x` without a token → 401.
- [ ] **Step 4: Commit** `Serve the region gateway on the nodes behind the Cloudflare proxy`.

---

### Task 7: The `kl` CLI

**Files:**
- Create: `bins/kl/Cargo.toml` (package `kl`, bin `kl`; deps `clap` (derive), `reqwest` (rustls, json), `tokio`, `tokio-tungstenite` (rustls), `serde`, `serde_json`, `dirs`, `open`), `bins/kl/src/{main.rs, config.rs, api.rs, login.rs, ws.rs, proxy.rs, sshconfig.rs}`, `bins/kl/tests/sshconfig.rs`, `.github/workflows/kl.yml`, `web/apps/web/public/install.sh`
- Modify: root `Cargo.toml` members

**Interfaces / behaviour:**
- `kl login [--api URL]`: `POST /v1/cli/code {device: <hostname>}` → prints the code and opens `<api-origin>/cli/authorize?code=…` (`open` crate; also prints the URL) → polls `GET /v1/cli/token?poll=` every 2 s up to 10 min → writes `{ api, token, expires_at, username }` to `~/.config/kl/config.json` (0600).
- `kl ws list [--team slug]`: table `NAME  ID  STATE  PACKAGES`.
- `kl ws ssh <name|id> [-- <ssh args>]`: resolve name → id via list; `POST …/ssh-session`; write host key to `~/.config/kl/known_hosts` as `<id> <host_key>`; exec `ssh -o ProxyCommand="kl ws proxy <id>" -o UserKnownHostsFile=~/.config/kl/known_hosts -o HostKeyAlias=<id> root@<id> <args>` (via `std::process::Command::exec` on unix).
- `kl ws proxy <id>`: mint a session; connect `wss://…/tunnel/<id>` with `Authorization: Bearer`; pump stdin→ws binary, ws binary→stdout; exit 0 on close, 1 on error with a one-line reason to stderr (never the token). On 401 from mint: "run `kl login`".
- `kl ws ssh-config`: writes `~/.ssh/kloudlite_config` with a `Host <name>` block per workspace (`HostName <id>`, `User root`, `ProxyCommand kl ws proxy <id>`, `UserKnownHostsFile ~/.config/kl/known_hosts`, `HostKeyAlias <id>`) and ensures `Include ~/.ssh/kloudlite_config` is the first line of `~/.ssh/config` (adds it if absent).
- `kl logout`: `DELETE /v1/cli/tokens/<jti>` then removes the config file.
- Workflow `kl.yml`: on tag `kl-v*`, build `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin` (cross via `cross` for linux arm64), upload to a GitHub Release with `sha256sums`. `install.sh`: detects OS/arch, downloads the latest release asset, installs to `~/.local/bin/kl` (or `/usr/local/bin` with sudo), prints the PATH hint.

- [ ] **Step 1: Failing tests**: `sshconfig.rs` — rendering is byte-exact for two workspaces; the Include line is added once (idempotent); `proxy` — a local WebSocket echo server: bytes written to the child's stdin come back on stdout (spawn the binary via `assert_cmd`-style `Command::cargo_bin("kl")` with a fake api URL pointing at a local axum stub that mints a fixed token).
- [ ] **Step 2: Run** → failures.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p kl && cargo clippy -p kl -- -D warnings`.
- [ ] **Step 5: Commit** `Add the kl CLI: login, list, ssh, ssh-config, proxy`.

---

### Task 8: Web — approve, tokens, snippet, key badge; public `/v1`

**Files:**
- Create: `web/apps/web/src/app/(shell)/cli/authorize/page.tsx` (+ action), `web/apps/web/src/components/app/cli-tokens.tsx`
- Modify: `web/apps/web/src/lib/api.ts` (`ApiWorkspace.ssh?: { gateway: string; host_key: string }`, `ApiCredential.material?: string`, cli token calls), `web/apps/web/src/app/(shell)/[owner]/(org)/settings/page.tsx` (CLI tokens section), `web/apps/web/src/components/app/workspace-list.tsx` (an `ssh` popover on the row: `kl ws ssh <name>` + "copy ssh config" of the Host block; hidden when `ssh` is absent), the keys settings component (badge "re-add to use for SSH" when `material` is empty), `deploy/rustic-git-web.yaml` (path rule `/v1/(.*)` → `rustic-git-api:80` before `/`, same rate-limit annotations)
- [ ] **Step 1**: tsc/lint-driven; a bun test for the ssh-config block renderer shared with the snippet.
- [ ] **Step 2–4**: implement; `bunx tsc --noEmit -p apps/web/tsconfig.json && bun run lint && bun test`; `kubectl apply --dry-run=client -f deploy/rustic-git-web.yaml`.
- [ ] **Step 5: Commit** `Approve CLI logins, list CLI tokens, show the ssh one-liner`.

---

### Task 9: e2e

**Files:** `tests/ws_e2e.sh`
- [ ] Add a phase (after the packages phase): mint a session via the api for `$WS_ID`; from the VM, `kl ws ssh $WS_ID -- true` using a config file pointing at the local api and a gateway reachable directly (`KL_GATEWAY_OVERRIDE=ws://<gateway-svc-ip>:8080` env honoured by `kl` for tests only, documented as such); a second user's token → mint 404; from a second workspace's pod, `nc -zw2 <first pod ip> 22` fails (NetworkPolicy).
- [ ] `bash -n`; commit `Exercise ssh into a workspace end to end`.

---

### Task 10: Rollout

1. Operator at Cloudflare: proxied A records `ws-centralindia-k3s.khost.dev` → `40.80.82.158`, `20.219.22.61`; SSL mode Full (strict); an Origin CA certificate for `*.khost.dev` installed as Secret `gateway-tls` in k3s `kube-system`; token permission *Zone → WAF: Edit* for the rate-limit rule.
2. k3s: `kubectl apply -f crds.yaml -f agent-rbac.yaml`; copy `rustic-git-jwt` Secret from AKS to k3s `kube-system`; `-f gateway.yaml -f cloudflared.yaml`; agent DaemonSet repin.
3. AKS: api/web repin; the `/v1` path rule.
4. Cloudflare rate limit on `/tunnel/*` (30/10 s per IP) once WAF: Edit is granted.
5. Prove: re-add an SSH key in the UI → `kl login` → `kl ws ssh gh -- git --version`; VS Code Remote-SSH to `gh`.

---

## Self-review

- Spec coverage: sessions (T1/T3), keys + authorized_keys (T2/T3), host keys + pod + policy (T4), gateway (T5), tunnel + Cloudflare (T6), CLI (T7), web + public /v1 (T8), e2e (T9), rollout (T10). Team members' keys, non-root user, custom images, session cut on key removal: out of scope per spec.
- Deviation from the spec, ruled: the gateway validates the session token **locally** with the shared JWT secret and tracks `jti` single-use in memory, instead of calling the api — no cross-cluster round trip on every connect, no api dependency for open sessions; the spec's "single use" holds per replica (2 replicas → a token can be used at most twice within 60 s, both by the same authorized holder). The spec's `GET /v1/ssh-sessions/{token}` is therefore not built.
- Types: `SshSessionClaims`/`CliClaims` (T1) used by T3/T5/T7; `Workspace.ssh` (T3) by T8; `ws-ssh-{id}` + `authorized_keys` (T3/T4) by T5's NetworkPolicy assumption (gateway label `app=rustic-git-gateway` in `kube-system`, T5/T4 agree).
