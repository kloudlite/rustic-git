# SSH into workspaces: sshd in the pod, an HTTPS gateway behind Cloudflare, a local CLI

**Status:** design, awaiting review. **Depends on:** workspace packages
(`2026-08-28-workspace-packages-design.md`) — `openssh` is in every workspace's base set, so
`sshd` is already on disk in every pod.

## Problem

The only way into a workspace today is `kubectl exec` with the cluster's kubeconfig — an
operator's tool, not a user's. People need `ssh`, and everything that rides on it (VS Code
Remote-SSH, scp, rsync, port forwards), for a workspace they own or a team workspace they may
act on — without the platform exposing an SSH port, a node address, or a region hostname that
anything but Cloudflare can reach.

## Decision, in one paragraph

Each workspace pod runs **`sshd`** from its Nix profile as the container's process, with a
per-workspace host key and the owner's registered public keys authorized. A **gateway** per
region (`kloudlite-gateway`, a small Rust service in the k3s cluster) accepts WebSocket
connections, authorizes them against the api, and pumps bytes to the workspace pod's port 22.
The gateway is reached **only through Cloudflare**: it serves TLS on the region nodes' public
interface with a Cloudflare Origin CA certificate, `ws-<region>.khost.dev` is a proxied hostname,
and the node firewall admits 443 from Cloudflare's published ranges only — no LoadBalancer, no
tunnel connector, the origin cannot be reached around the edge, and Cloudflare's WAF/rate
limiting/DDoS absorption front the gateway. A **local
CLI** (`kl`) logs in once, then runs as ssh's `ProxyCommand`: `kl ws ssh gh` is
`ssh -o ProxyCommand="kl ws proxy ws-…" kl@gh`. The api mints a short-lived **session token**
per connection (`POST /v1/workspaces/{id}/ssh-session`), which is the only credential the
gateway ever sees. Tenancy is enforced twice: the gateway checks the token against the
workspace, and the pod's sshd checks the user's own key.

Not chosen: a public SSH port per workspace or per node (scannable, node-bound, exposes sshd to
the internet); an SSH gateway on a public IP (a listener, even key-only, is a target and needs a
region hostname); WireGuard (right latency, wrong amount of code for release 1); relaying bytes
through `dev.kloudlite.io`/AKS (the extra hops the owner rejected — here `dev.kloudlite.io` only
mints the session token, the data path is user → nearest Cloudflare PoP → region tunnel → pod).

## Components

| Component | Where | What it does |
|---|---|---|
| `sshd` in the workspace pod | every workspace pod (k3s) | `/nix/profile/current/bin/sshd -D -e -f /etc/ssh/sshd_config` as the container command for the default image; config + host keys from a per-workspace Secret; `authorized_keys` from the owner's registered keys. |
| `kloudlite-gateway` | `bins/gateway`, Deployment in k3s (`kube-system`, 2 replicas) | HTTP server: `GET /tunnel/{ws-id}` upgrades to WebSocket; validates the session token with the api; resolves the pod IP from the Workspace's status; dials `pod:22`; pumps. No shell, no auth of its own, no state. |
| Cloudflare | edge | Proxied A records for `ws-<region>.khost.dev` → the region nodes' public IPs; Origin CA certificate on the gateway; SSL mode Full (strict); WAF managed rules; rate limiting on `/tunnel/*`. |
| api (`bins/api`) | AKS | `POST /v1/workspaces/{id}/ssh-session` (mint), `GET /v1/ssh-sessions/{token}` (gateway validation), SSH key material stored on credentials, `authorized_keys` written into the `user-key` Secret; `/v1` made public (path rule on `dev.kloudlite.io`). |
| agent (`bins/agent`) | k3s | Per-workspace `ws-ssh-{id}` Secret (host keys + `sshd_config`), pod command/mounts, NetworkPolicy allowing 22 from the gateway only. |
| `kl` | `bins/kl`, user's machine | `login`, `ws list`, `ws ssh`, `ws ssh-config`, `ws proxy`; the last is a stdio↔WebSocket pump. |
| web | AKS | The workspace row shows the one-liner and a "copy ssh config" button. |

## Data model

### Sessions (api, in-memory + Redis)

```
POST /v1/workspaces/{id}/ssh-session      Authorization: Bearer <cli or session JWT>
→ 201 { "token": "sst_…", "gateway": "wss://ws-centralindia-k3s.khost.dev/tunnel/ws-…",
        "expires_at": "…", "host_key": "ssh-ed25519 AAAA… ws-…" }
```
- `token`: 32 random bytes, base64url, prefixed `sst_`. Stored in Redis as
  `sshsess:{token} → {workspace, owner, expires}` with a 60 s TTL: it is a *connect* token, spent
  by the gateway on upgrade. A connection outlives its token; a reconnect mints a new one (the CLI
  does this transparently).
- Authorization at mint: `may_act_on(caller, workspace.owner/team)` — personal owner, or team
  member for a team workspace. Workspace must be `ready`.
- `host_key`: the pod's public host key, so the CLI can pin it in `known_hosts` on first use
  (the CLI passes `-o UserKnownHostsFile` entries it manages; no TOFU prompt for a key the
  platform already knows).
- Redis is a cache here, not the record — if it is down, mint fails closed (503) and nobody
  connects; nothing is lost.

### Validation (gateway → api)

```
GET /v1/ssh-sessions/{token}                 X-Gateway-Region: centralindia-k3s + region agent token
→ 200 { "workspace": "ws-…", "owner": "…" }   (and the token is deleted: single use)
→ 404 unknown/expired/used
```
The gateway authenticates to the api with the region's agent token (already exists, source-bound
to the region's IPs — the gateway runs on those nodes, so the binding holds). The api replies
with what the gateway must pin the connection to.

### Keys

- **Registered SSH keys gain material.** `Credential.material` is set for `kind=SshKey` from now
  on (`credentials.rs` keeps it empty today; GPG keys already keep theirs). Keys added before this
  ship are shown in the UI with "re-add to use for SSH" — there is no way to recover the public
  key from a fingerprint.
- **`authorized_keys`** = the owner's SSH-key material lines, written by the api into the
  existing `user-key` Secret (`install_user_key`) under a second key `authorized_keys`, and
  re-written on every credential add/remove for that owner (the api already owns that Secret).
  Team workspaces: the *workspace owner's* keys only, release 1; members reach it with their own
  keys in release 2 (needs a per-workspace union, and a UI to say who can ssh).
- **Host keys**: the agent generates an ed25519 pair per workspace on first reconcile into Secret
  `ws-ssh-{id}` in the workspace namespace (`ssh_host_ed25519_key`, `.pub`, `sshd_config`).
  ownerReference → Workspace, so it dies with it; survives pod recreation, clones get a new one.
  The public half is copied to `status.sshHostKey` so the api can hand it to the CLI.

### Pod

For the default image, the container command becomes
`/nix/profile/current/bin/sshd -D -e -f /etc/ssh/sshd_config` (replacing `sleep infinity`).
`/etc/ssh` is the `ws-ssh-{id}` Secret (read-only); `/home/kl/.ssh/authorized_keys` is a subPath of
the `user-key` Secret (read-only, mode 0600). `sshd_config`:

```
Port 22
HostKey /etc/ssh/ssh_host_ed25519_key
PermitRootLogin no
AllowUsers kl
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AuthorizedKeysFile /home/kl/.ssh/authorized_keys
AllowTcpForwarding yes
X11Forwarding no
ClientAliveInterval 30
Subsystem sftp /nix/profile/current/libexec/sftp-server
```
A user image keeps its own entrypoint (as today) — ssh is a feature of the default image in
release 1; `spec.ssh: true` for custom images is a follow-up. The pod runs as root (alpine) —
`PermitRootLogin prohibit-password` is what makes that acceptable; a `dev` user is release 2.

**NetworkPolicy**: the namespace's default-deny ingress gains one rule — port 22 from pods with
label `app=kloudlite-gateway` in `kube-system`. Nothing else in the cluster can reach a
workspace's sshd, including other tenants and other workspaces of the same tenant.

### Gateway

- `GET /tunnel/{ws-id}` with `Authorization: Bearer sst_…` → validate with the api (single use,
  must name the same `ws-id`) → look up the Workspace (`status.podRef`, must be `ready`) → dial
  `pod-ip:22` (the pod IP from the Pod object; the gateway has RBAC `get` on pods in `ws-*`
  namespaces and on workspaces) → `101 Switching Protocols` → binary frames both ways until
  either side closes. Failure before upgrade is a plain HTTP status (401/404/409 not ready/503).
- Limits: one dial per upgrade, idle timeout 30 min without frames (ssh keepalives keep it
  alive), 64 KiB frames, 10 concurrent tunnels per workspace, 100 per owner (counted in memory
  per replica — ponytail: per-replica, not global; fine at 2 replicas).
- TLS on 443 with a Cloudflare Origin CA certificate (Secret `gateway-tls`), bound to the node's
  public interface via `hostPort`, one replica per pool node; the node firewall admits 443 from
  Cloudflare's published ranges only, so the edge is the only client that can complete a
  handshake. Plain HTTP on 8080 for the cluster-internal health check and tests.
- Stateless; logs `owner, workspace, bytes, duration` per session, never the token.

### Cloudflare

- DNS: `ws-<region>.khost.dev` **A** records to each pool node's public IP, proxied. Adding a
  node is adding a record; a dead node's record is removed (Cloudflare does not health-check
  origins on the free plan, so a dead A record is a failed connect until it is removed —
  `kl` retries once, which covers a single bad pick).
- SSL/TLS mode Full (strict) on the zone; the gateway presents a Cloudflare Origin CA
  certificate (dashboard: SSL/TLS → Origin Server → Create Certificate, `*.khost.dev`, 15 years)
  installed as Secret `gateway-tls`. Browsers never see it; only the edge does.
- Rate limiting (needs *Zone → WAF: Edit*): `/tunnel/*` 30 req/10 s per IP (a connect is one
  request; a reconnect storm is not). WAF managed ruleset on.
- WebSockets are on for the zone by default; the edge idles a WebSocket after 100 s without
  traffic — ssh's `ClientAliveInterval 30` keeps every session under that.

### CLI (`kl`)

Rust, `bins/kl`, built by CI for macOS (arm64/x86_64) and Linux (x86_64/arm64), released as
GitHub release assets; install: `curl -fsSL https://dev.kloudlite.io/install.sh | sh`
(release 2: brew tap).

```
kl login [--api https://dev.kloudlite.io]   # opens the browser to /cli/authorize?code=XXXX;
                                            # polls /v1/cli/token until approved; stores
                                            # ~/.config/kl/config.json (0600): api, token, expiry
kl ws list                                  # id, name, state, packages
kl ws ssh <name|id> [-- ssh args…]          # mint session → ssh -o ProxyCommand … kl@<id>
kl ws proxy <id>                            # ProxyCommand: mint session, open wss, pump stdio
kl ws ssh-config                            # writes ~/.ssh/config.d/kloudlite (and an Include)
                                            #   Host gh  → HostName ws-…, User kl,
                                            #   ProxyCommand kl ws proxy ws-…, UserKnownHostsFile …
kl logout
```
- The CLI token is a JWT with `typ: cli`, 30 days, revocable (`/v1/cli/tokens` list/delete, shown
  in the UI under Settings → CLI). It is what `ssh-session` is minted with.
- `kl ws proxy` retries the mint once on 401 (expired CLI token → prints "run `kl login`").
- `known_hosts`: the CLI keeps `~/.config/kl/known_hosts` with the platform-provided host key per
  workspace id, passed via `-o UserKnownHostsFile`; a changed key (restore into a new workspace
  keeps its own) is just a new id.
- Team workspaces appear in `kl ws list --team <slug>`.

### api: public `/v1`

Today `/v1` is reachable only inside AKS. The web ingress gets a path rule
`/v1/(.*) → kloudlite-api` on `dev.kloudlite.io` (regex already enabled), with the same
per-IP rate limit as the app. `/v1/cli/*` and `/v1/workspaces/{id}/ssh-session` are the routes
the CLI uses; everything else on `/v1` is what the web already calls server-side, now also
callable by the CLI with the same bearer semantics.

## Flows

**First-time setup:** add an SSH key in the UI (stores material; api rewrites `authorized_keys`
into the namespace Secret) → `kl login` → `kl ws ssh gh`.

**Connect:** CLI mints a session (api checks `may_act_on`, workspace ready) → ssh starts with the
ProxyCommand → CLI opens the wss with the session token → Cloudflare → tunnel → gateway validates
(single use) → dials pod:22 → ssh handshake runs end-to-end through the pump → sshd checks the
user's key. Everything after the upgrade is opaque ssh bytes; the gateway and Cloudflare see
ciphertext.

**Workspace stops/restarts:** sshd dies with the pod; the session drops; `kl` prints the state.
A recreated pod has the same host key (Secret), so `ssh` reconnects without a warning.

**Key removed in the UI:** api rewrites `authorized_keys`; sshd reads the file per login, so the
next login fails; an open session is not cut (release 1 — killing sessions on key removal needs
the gateway to track owner → sessions; noted).

**Clone / restore:** a clone gets its own host key and inherits `authorized_keys` (same owner);
a restore into a new workspace likewise. An in-place restore keeps the Secret (it is not on the
subvolume).

## Security

- One inbound port: 443 on the pool nodes, admitted from Cloudflare's published ranges only
  (`harden-node.sh`, `CF_CIDRS`). Cloudflare fronts the gateway with WAF, rate limits and DDoS
  absorption; the origin cannot be reached around the edge, and a handshake from anywhere else
  is dropped before TLS.
- The gateway holds no user credential: it sees a 60 s single-use connect token, and after the
  upgrade only ciphertext. It cannot open a session on its own (sshd wants the user's key).
- Two independent authorizations: api (`may_act_on`) at mint, sshd (`authorized_keys`) at login.
- Pod isolation unchanged: gVisor, no SA token, egress denylist; sshd listens on the pod IP,
  reachable only from the gateway label by NetworkPolicy.
- Host keys are per workspace and never leave the cluster except as the public half.
- Tokens never logged. CLI config is 0600. Logout deletes the CLI token server-side.
- Abuse limits: per-IP at Cloudflare, per-workspace/owner concurrency at the gateway, 60 s
  connect-token TTL.

## Failure modes

| Failure | Behaviour |
|---|---|
| Cloudflare or tunnel down | `kl` reports "gateway unreachable"; nothing else affected; workspaces keep running. |
| api down | no new sessions (mint 503); open sessions unaffected (the gateway never re-validates). |
| Redis down | mint fails closed (503). |
| workspace not ready | mint 409 with the state; `kl` prints it and exits 1. |
| pod recreated mid-session | session drops; reconnect reuses the same host key, no warning. |
| expired CLI token | 401 → "run `kl login`". |
| key added before material was stored | UI marks it "re-add to use for SSH"; `authorized_keys` omits it. |

## Testing

- gateway: unit tests for the authorization path against a mocked api (single use, wrong
  workspace, expired) and a pump test (a local TCP echo behind a WebSocket).
- api: mint/validate tests (may_act_on, ready gate, TTL, single use), `authorized_keys`
  rewrite on key add/remove, CLI token issue/revoke.
- agent: `ws-ssh-{id}` Secret created with ownerReference, pod command/mounts for the default
  image, NetworkPolicy shape (k8s.rs tests).
- kl: `ssh-config` rendering, config file permissions, proxy pump against a local WebSocket
  echo.
- e2e (`tests/ws_e2e.sh`, on the VM): `kl ws ssh <id> -- true` succeeds; a second user's CLI
  token gets 404 on mint; port 22 unreachable from another workspace's pod.

## Rollout

1. api: key material + `authorized_keys` rewrite + CLI tokens + session endpoints; `/v1` public
   path rule. (Nothing user-visible changes yet.)
2. agent: `ws-ssh-{id}` Secrets, pod command/mounts, NetworkPolicy. Existing default-image pods
   get sshd on their next recreate (stop/start), as with every pod-spec change.
3. gateway + cloudflared + tunnel/DNS in the region; Cloudflare rate limit on `/tunnel/*`.
4. `kl` first release; UI one-liner + "copy ssh config"; Settings → CLI tokens.

## Out of scope (deliberately)

Non-root user in the pod; ssh for custom images; team members' keys on team workspaces;
cutting live sessions on key removal; a web terminal (would reuse the gateway); WireGuard.
