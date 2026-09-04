# Security audit — kloudlite-git

Scope: every HTTP surface (server public+peer listeners, `/v1` on `bins/api`, `/v2` registry, gateway `/tunnel`, web app), JWT (`crates/core/src/jwt.rs`), credential storage, secrets in logs/argv, path/digest/name validation, SSRF, body limits, transport, k8s RBAC/pod security/NetworkPolicy, workspace sshd/prelude, Cloudflare origin lock, Cosmos/Mongo, Cargo/bun deps, CI. Code was read, not comments; the four highest-ranked code findings were re-verified by hand. No files modified.

Totals: critical 0, high 4, medium 9, low 27 (40).

---

### [S-1] Workspace namespace name collides across (team, owner) pairs — one user's private git key lands in another user's pod
Severity: high
Location: `crates/workspaces/src/crd.rs:610-617` (`ws_namespace`), `crates/workspaces/src/api.rs:686,728-758` (`write_user_key`), `crates/workspaces/src/k8s.rs:430-452,818-828`; handle charset `crates/pulls/src/directory/mod.rs:270`
What: `ws_namespace` is `ws-{team}-{owner}` / `ws-{owner}` joined by `-`, and handles and team slugs both permit `-`. Team `b` + owner `c` = `ws-b-c` = personal namespace of owner `b-c`; team `a-b`/owner `c` = team `a`/owner `b-c`. Both controllers create the same namespace; `write_user_key` force-applies a Secret with the fixed name `user-key` (owner's `id_ed25519` + `authorized_keys`), and every pod in the namespace mounts it. `binding_name` (`crd.rs:596-598`, `{region}-{owner}`) has the same ambiguity.
Why it matters: An attacker who creates a team with a chosen slug can target any handle containing a dash: their pod reads the victim's platform private key (mode 0444) and the victim's sshd accepts the attacker's `authorized_keys`. `owners_namespaces` (`api.rs:722`) treats the shared namespace as "mine" for both.
Fix: Use a join no handle can produce. Handles are `[a-z0-9-]`, so encode with a fixed-width hash tail for team namespaces: `format!("ws-{owner}-{}", &sha256(team)[..8])` (dns_label already has hash logic for long names — reuse it), same for `binding_name`. Add a unit test `ws_namespace("c","b") != ws_namespace("b-c","")` and `ws_namespace("c","a-b") != ws_namespace("b-c","a")`. Existing namespaces need a one-time migration (controller re-creates on next reconcile).
Effort: S (code) / M (migration)

### [S-2] Cloudflare "Flexible" SSL: every credential crosses edge→origin in cleartext
Severity: high
Location: `deploy/kloudlite-git-web.yaml:127-141`, `deploy/kloudlite-git.yaml:914-918,935-937`, `deploy/k3s/gateway.yaml:5-6`
What: All three public hostnames (app, registry, workspace gateway) terminate TLS at Cloudflare and reach the origin over plain HTTP (`ssl-redirect: "false"`, gateway on hostPort 80). The registry ingress already has a cert-manager cert that Cloudflare never uses. `deploy/k3s/README.md:111-113` documents the Full-strict + Origin CA path for the gateway; it is not applied and AKS has no equivalent.
Why it matters: Git Basic-auth passwords, registry bearer tokens, Auth.js session cookies, the region agent token (`/vol-agent`) and gateway ssh-session JWTs are all observable on the edge→Azure LB hop. The origin lock limits who can connect, not who can observe.
Fix: Zone SSL mode → Full (strict); Cloudflare Origin CA cert in the two AKS ingress `tls:` blocks (registry's secret slot exists) with `ssl-redirect: "true"`; create `gateway-tls` and set `GATEWAY_TLS_DIR`.
Effort: M

### [S-3] Git HTTP handlers buffer up to 2 GiB per request *before* authentication
Severity: high
Location: `bins/server/src/router/git.rs:208,246,343` (`body: Bytes`, `DefaultBodyLimit::max(max_body())`), `crates/core/src/httpx.rs:73-81`
What: `upload_pack` and `receive_pack` take `Bytes`, so axum reads the whole body (cap `max_body`, default 2 GiB) before `open()` runs authn/authz. Routing only requires the repo to exist. The `httpx.rs:73` comment claims an anonymous client cannot make the server buffer "more than this" — "this" is 2 GiB per request.
Why it matters: A handful of concurrent anonymous `POST /{o}/{n}/git-receive-pack` bodies OOMs a node with fixed memory limits, and the StatefulSet roll then moves DB ownership (the fenced-handle gap in CLAUDE.md).
Fix: Take `axum::body::Body`, call `open()` first, then `axum::body::to_bytes(body, max_body())` for upload-pack; for receive-pack stream into `write_pack` (already a `BufRead` consumer) via `tokio_util::io::StreamReader` instead of `Cursor<Bytes>`. Keep the `// ponytail:` marker.
Effort: M

### [S-4] Cloudflare origin lock is a hand-run, unversioned `kubectl patch` with no drift check
Severity: high
Location: `deploy/ingress-nginx-origin-lock.md:10-11`, `deploy/kloudlite-git.yaml:864-868`, `deploy/ingress-nginx-config.yaml:19-21`
What: The only thing stopping direct-to-LB traffic is `spec.loadBalancerSourceRanges` patched by hand onto `svc/ingress-nginx-controller`, which is not in the repo. Nothing asserts it is set.
Why it matters: Without it, WAF/rate limits are bypassed and `X-Real-IP` becomes attacker-chosen — which is what the registry rate-limit whitelist (`kloudlite-git.yaml:932`) and the `/vol-agent` region source binding (`bins/server/src/vol_agent.rs:110`) trust. A Helm upgrade or reinstall of ingress-nginx silently drops it.
Fix: Commit `deploy/ingress-nginx-service.yaml` (or a Helm values file) carrying `loadBalancerSourceRanges` generated from `cloudflare-ips-v4.txt`; add one line to the deploy notes: `kubectl -n ingress-nginx get svc ingress-nginx-controller -o jsonpath='{.spec.loadBalancerSourceRanges}'` must be non-empty before a roll.
Effort: S

### [S-5] Per-owner blob store: one public image exposes every private image's layers under that owner
Severity: medium
Location: `crates/registry/src/blobs.rs:57-63` (`allow(owner,name,false)` then `blob_path(&owner,&d)`), `crates/registry/src/store.rs:120`
What: Authorization is per image; storage is per owner (`blobs/{owner}/{algo}/{hex}`). A caller allowed to pull one public `acme/*` image can `GET/HEAD /v2/acme/<public>/blobs/<digest>` for any digest that exists only in `acme/<private>`. HEAD is an existence oracle, GET returns bytes.
Why it matters: Layer digests are widely known (base images, CI logs, SBOMs). Publishing one public image makes every private layer of that owner readable by digest.
Fix: Write an `image/blob/{digest}` row in the image's own DB at `finish_blob`/`complete`/mount; in `blob_response`, when the caller is not the owner (or a member), require that row to exist. Owner/member callers keep the current path. GC already lists per-manifest, so no change there.
Effort: M

### [S-6] GPG "verified" badge never ties the key to the user who registered it — authorship spoofing
Severity: medium
Location: `crates/api/src/signatures.rs:179-207` (`judge_pgp`) vs `judge_ssh:231`
What: `judge_ssh` requires `known.created_by == author_email`. `judge_pgp` only checks that the key's *self-signed uid* contains the author email (`gpg::verify(.., &signed.author_email)`), and `add_key` accepts any armour.
Why it matters: Anyone can generate a key with uid `victim@example.com`, register it, and commits authored as the victim render `verified`.
Fix: In `judge_pgp`, before returning `verified`: `if !known.created_by.eq_ignore_ascii_case(&signed.author_email) { return bad_email(..) }` — the same line `judge_ssh` already has.
Effort: S

### [S-7] Ref names are validated only on the push path; browse-API writes create arbitrary ref names
Severity: medium
Location: `crates/gitbase/src/refs.rs:75-89` (`update_refs`, no check); callers `bins/server/src/browse_api/merge.rs:112,277,381-383`; `valid_ref_name` only at `crates/git/src/protocol/receive.rs:185`
What: A repo member can `POST /v1/repos/{o}/{n}/commits` with `newBranch: "x\n<oid> refs/heads/main"` (or NUL, spaces, `..`, `.lock`, 10 KB). The api tier forwards the body verbatim (`crates/api/src/signatures.rs:41-95`). The ref is written and then emitted raw in `receive::advertise` / `ls_refs` pkt-lines.
Why it matters: Repo-wide DoS (every clone/fetch gets a corrupt advertisement) and name injection into the protocol stream, by any member.
Fix: Move `valid_ref_name` into `gitbase` and call it in `update_refs` for every `RefUpdate.name`; delete the copy in `receive.rs`. One place covers push, patch, merge.
Effort: S

### [S-8] Agent DaemonSet SA can rewrite Workspace/Environment spec and read any Secret cluster-wide
Severity: medium
Location: `deploy/k3s/agent-rbac.yaml:34-36,79-81,92-101`
What: `patch` on the main `workspaces`/`environments` resources (for `heal_labels`), not just `/status`; `secrets: get, create` cluster-wide with no `resourceNames`; `create rolebindings` anywhere and `bind` on `kloudlite-git-api-secrets`. The yaml's own comments (`:10-11,31-33`) acknowledge both.
Why it matters: The pod is privileged so a compromise owns its node regardless — but with this RBAC one compromised node can `get secret kloudlite-git-jwt` (gateway signing key) and the api's credentials, i.e. every node and every tenant. CLAUDE.md's "RBAC — not convention — stops a controller editing desired state" is not currently true.
Fix: (a) `ValidatingAdmissionPolicy` rejecting any patch from `kube-system:kloudlite-git-agent` that touches anything outside `metadata.labels`/`metadata.finalizers` (named in the comment at `:31-33`). (b) VAP restricting `get secrets` to names with prefix `ws-ssh-`, or generate host keys inside the workspace namespace under the per-namespace Role that already exists.
Effort: M

### [S-9] `region` is free text on create; drives object names and the gateway URL
Severity: medium
Location: `crates/workspaces/src/api.rs:530,621,1120,1207,1284`; consumers `crates/workspaces/src/crd.rs:596-598`, `bins/agent/src/claim.rs:206-218`, `api.rs:820-824`
What: Never checked against `store.regions()`. `binding_name` = `{region}-{owner}`, so attacker `att` with `region: "centralindia-x"` pre-creates OwnerBinding `centralindia-x-att`, which is exactly what victim `x-att` in region `centralindia` needs; `ensure_binding` treats the 409 as success and `namespace_ready` (`binding.rs:1056-1061`) reads the attacker's `NamespaceReady`. A region with `/` or `.` is echoed into `wss://ws-{region}.khost.dev`.
Why it matters: Victim's workspaces proceed against a namespace that was never made (permanent `Creating`); cross-tenant object squatting.
Fix: In `create_ws`/`create_env`/`restore_env`, `require region ∈ active s.store.regions() ids` (422 otherwise); fix `binding_name` with S-1's hash tail.
Effort: S

### [S-10] `spec.quota_gb` is never enforced on disk — any tenant can fill a node's btrfs pool
Severity: medium
Location: `crates/workspaces/src/engine/ops.rs:211-222` (`create_subvol`), `bins/agent/src/controller.rs:1557-1579`, `crates/workspaces/src/api.rs:531,1131`
What: No `btrfs qgroup` anywhere; the local PV `capacity` is nominal; `quota_gb` is user-supplied and unbounded (0 or 10^12 both accepted).
Why it matters: One tenant writing until ENOSPC takes every workspace/env on that node down (`set_lineage` and pushes fail).
Fix: `btrfs quota enable` once per pool (`format-pool.sh`); after `create_subvol`/`pull_core`, `btrfs qgroup limit {quota}G {path}`; clamp `quota_gb` to `1..=500` in the two create routes.
Effort: M

### [S-11] AKS has no NetworkPolicy enforcement; the peer listener is protected by one static secret shared across four tiers
Severity: medium
Location: `deploy/kloudlite-git.yaml:113-114,363-364,578-579`; `crates/core/src/peer.rs:11-13`
What: The manifest states `networkPolicy: none`; `kloudlite-git-peers-only` is decorative. Peer ports 8081/8082 (ref moves, merge claims, ownership map, unthrottled `/api/`) are reachable from every pod in the cluster, gated by `KLOUDLITE_GIT_PEER_SECRET`, which lives in web, api, worker and server pods.
Why it matters: RCE in the Next.js tier (widest surface) is one `curl` from moving a ref on any repository; rotation means restarting every tier.
Fix: Enable network policy on the AKS cluster (`az aks update --network-policy azure` / Cilium) so the existing policy takes effect; add default-deny in the `kloudlite-git` namespace.
Effort: M

### [S-12] `client_ip` falls back to the client-controlled first `X-Forwarded-For` hop
Severity: medium
Location: `bins/server/src/vol_agent.rs:246-252`, used by `authorized_for` at `:110`; test at `:323` asserts the forgeable behaviour
What: Prefers `X-Real-IP` (nginx-set) but falls back to the first XFF element, which Cloudflare and ingress-nginx (`use-forwarded-headers: "true"`) both append to, never replace.
Why it matters: Dead code on the ingress path today, but any path reaching the public listener without nginx (NodePort, in-cluster Service, second ingress, S-4 lapsing) makes `KLOUDLITE_GIT_AGENT_SOURCES` a one-header bypass for a leaked region token.
Fix: Delete the `.or_else(x-forwarded-for ..)` branch; `None` already fails closed for bound regions (`:256-258`). Flip the test.
Effort: S

### [S-13] Web app sends no security response headers
Severity: medium
Location: `web/apps/web/next.config.ts:3-21` (no `headers()`), `deploy/kloudlite-git-web.yaml:140-159`
What: No CSP, no `X-Frame-Options`/`frame-ancestors`, no `X-Content-Type-Options`, no `Referrer-Policy`, no HSTS; `X-Powered-By: Next.js` emitted. Confirmed live against a dev build.
Why it matters: Passkey-approve, CLI-approve and delete buttons are frameable (clickjacking); a future XSS has no CSP backstop; HSTS depends entirely on Cloudflare config nothing in the repo asserts.
Fix: `headers()` in `next.config.ts` returning `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: strict-origin-when-cross-origin`, `Strict-Transport-Security: max-age=31536000; includeSubDomains`, `Content-Security-Policy: frame-ancestors 'none'; object-src 'none'; base-uri 'self'`; `poweredByHeader: false`.
Effort: S

### [S-14] `restore_env` lets a member move a snapshot from team A into team B
Severity: low
Location: `crates/workspaces/src/api.rs:1262-1265`
What: `find_snapshot` resolves under any team the caller belongs to, then `resolve_new_owner` accepts any *other* team the caller belongs to.
Why it matters: Team A's data becomes a team-B environment readable by all of B; the copy escapes A's membership boundary.
Fix: Require `body.owner == src_owner || body.owner == caller`.
Effort: S

### [S-15] `/v1/volumes/{name}` and `{snapshot}` path params are spliced into peer-listener URLs unvalidated
Severity: low
Location: `crates/workspaces/src/upstream.rs:58,86,96,122`; callers `crates/workspaces/src/api.rs:1783,1800,1813,1845`
What: axum percent-decodes `Path`; reqwest/url resolves `..`, so `snapshot = "..%2F..%2F<x>%2Fvolumedelete"` turns `DELETE .../snapshotdelete/{snapshot}` into `DELETE /api/{owner}/<x>/volumedelete`. `OWNER_HEADER` is the caller's verified owner, so no cross-tenant reach, but every peer route becomes callable through `/v1` with a surprise handler.
Fix: In `volume_owner`, `delete_snapshot`, `find_snapshot` refuse anything failing `kloudlite_git_storage::store::valid_segment` before calling `Upstream`.
Effort: S

### [S-16] Environment `name`, service names, env-var keys and ports are unvalidated
Severity: low
Location: `crates/workspaces/src/api.rs:1119-1132,1200-1215`; `crates/workspaces/src/k8s.rs:922,936-950,967,987,1013,1023-1030`
What: Workspace `name` goes through `check_ws_name`; environment `name` does not. A service named `Foo_bar` or 300 chars is a 422 from the API server on every reconcile forever; duplicate service names silently overwrite a sibling; port 0 accepted.
Fix: Validate `name` with `valid_ws_name`, each `svc.name` as RFC-1123 label + unique, `ports` in `1..=65535`, env keys as `[A-Za-z_][A-Za-z0-9_]*`.
Effort: S

### [S-17] Push error report echoes internal backend errors to the client
Severity: low
Location: `crates/git/src/protocol/receive.rs:129-134`; `crates/git/src/ssh.rs:139,206,255`
What: `unpack_status = format!("error {m}")` where `m` comes from `store.upload_pack_files` (object_store errors carry bucket/key/URL) and `update_refs` (SlateDB text). SSH path sends `{e}` from `open_repo_after_fence`.
Fix: Log `m`; send a fixed `unpack failed: internal error` unless it is a client-side pack error from `write_pack`.
Effort: S

### [S-18] Owner/repo/image names have no length cap
Severity: low
Location: `crates/storage/src/store.rs:215-221` (`valid_segment`); `crates/api/src/repos.rs:126`
What: A thousand-char name passes, exceeds S3's 1024-byte key limit and local PATH_MAX (`store.rs:369`), leaving half-created state (marker written, DB dir failed).
Fix: `&& s.len() <= 100` in `valid_segment`.
Effort: S

### [S-19] Signing/access key squatting on someone else's public key
Severity: low
Location: `crates/api/src/credentials.rs:341-356`
What: Credential id is the bare fingerprint (`sign:{fp}`), globally unique, no possession proof. Registering another person's public key first 409s their own later `add_key`; for signing keys their commits then show `bad_email` signed by the squatter.
Fix: Scope signing-key ids by owner (`{owner}:sign:{fp}`); for access keys keep global uniqueness (the fleet map needs it) but return the same 409 only when the fingerprint maps to a different owner.
Effort: M

### [S-20] Unbounded anonymous flood surfaces
Severity: low
Location: `crates/api/src/credentials.rs:447-485` (`POST /v1/cli/code`), `crates/api/src/teams.rs:799-848` (`create_signin_link`), `crates/app/src/lib.rs:181-188` (neg-route cache `retain` only drops expired)
What: One Mongo row per anonymous `/cli/code` call (10-min TTL) with no per-IP cap; magic-link mail volume per address bounded only by the ingress; distinct bad repo names within `NEG_TTL` grow the negative cache without bound.
Fix: Per-IP/token bucket at the ingress for the first two (or a keyed counter); cap `neg_cache` at N entries, evict oldest.
Effort: S each

### [S-21] Emailed sign-in link is a GET that logs the browser in (login CSRF)
Severity: low
Location: `web/apps/web/src/app/(auth)/verify/[token]/route.ts:17-28`
What: Visiting `/verify/{token}` redeems and sets the session cookie with no user gesture. An attacker requests a link for their own address and gets the victim to open it; the victim is now signed into the attacker's account and may paste keys/tokens into it.
Fix: Render a one-button form that POSTs to a server action doing redeem + `signIn`; keep `safeNext`.
Effort: S

### [S-22] Team `website` stored and rendered as raw `href` with no scheme check
Severity: low
Location: `web/apps/web/src/app/(shell)/[owner]/(org)/settings/actions.ts:149`, `web/apps/web/src/components/app/team-profile.tsx:77`, `crates/api/src/teams.rs:509`
What: Neither tier validates the URL. React 19's `sanitizeURL` blocks `javascript:` today; `data:`, `vbscript:`, `file:` pass onto an anonymous public page.
Fix: In `saveProfile` (and `update_team`): `new URL(website)` and require `https?:`.
Effort: S

### [S-23] Shared preview password has no brute-force limit
Severity: low
Location: `web/apps/web/src/auth.ts:12-36`, `web/apps/web/src/app/(auth)/login/actions.ts:70-92`
What: With `AUTH_ALLOWED_EMAILS` + `AUTH_SHARED_PASSWORD`, guesses are limited only by ingress `limit-rps: 30`; one secret covers every allow-listed account.
Fix: Unset in production, or a per-email in-memory failure counter with backoff in `previewCredentials().authorize`.
Effort: S

### [S-24] Magic-link sending has no per-address cooldown
Severity: low
Location: `web/apps/web/src/app/(auth)/login/actions.ts:41-66`
What: Any syntactically valid address gets a token and a mail per call.
Fix: 60 s per-email cooldown in the api's `/v1/signin/email` (single writer).
Effort: S

### [S-25] WebAuthn rpID/origin derived from `X-Forwarded-Host`/`Host` rather than `AUTH_URL`
Severity: low
Location: `web/apps/web/src/lib/passkey.ts:24-34`
What: Safe behind ingress-nginx today; any direct exposure of the pod lets a caller register/verify passkeys for an arbitrary rpID.
Fix: Prefer `new URL(process.env.AUTH_URL).host`, headers only in dev.
Effort: S

### [S-26] `next-auth@^5.0.0-beta.32` floats on a pre-release line
Severity: low
Location: `web/apps/web/package.json:19`
What: The whole session layer rides a beta with a caret; the app already pins cookie names by hand to cope (`auth.ts:127-136`).
Fix: Pin exactly (`5.0.0-beta.32`); track v5 GA.
Effort: S

### [S-27] Images tagged `:latest` on every push and pinned by mutable SHA tag, tests do not gate build
Severity: low
Location: `.github/workflows/image.yml:39-42,69,86,98`; `web.yml:61`; `deploy/*.yaml` image lines
What: `build` has no `needs: test`; `:latest` moves on every push; SHA *tags* are mutable by anyone with `packages: write`.
Fix: Drop the `:latest` lines (nothing in-repo consumes them); pin manifests by `@sha256:` digest from `build-push-action`'s `digest` output.
Effort: S

### [S-28] No `permissions:` block on `web.yml` check job and `kl.yml` build job
Severity: low
Location: `.github/workflows/web.yml:15-18`, `.github/workflows/kl.yml:9-26`
What: Inherit the repo default token while running `bun install` lifecycle scripts (`sharp`, `unrs-resolver` in `trustedDependencies`) from a branch-controlled lockfile.
Fix: `permissions: { contents: read }` at the top of both workflows.
Effort: S

### [S-29] gVisor installed from `release/latest` with same-origin checksum, and not enabled
Severity: low
Location: `deploy/k3s/install-gvisor.sh:16,22-26`; `deploy/k3s/agent-daemonset.yaml` (no `WS_RUNTIME_CLASS`)
What: Unpinned binary that is the tenant/kernel boundary; version skew across nodes; tenants currently share the host kernel under runc.
Fix: Pin to a dated release and record the sha512 in the script; set `WS_RUNTIME_CLASS=gvisor`.
Effort: S

### [S-30] Control-plane backup bundles cluster CA + node token under fixed names with a long-lived SAS
Severity: low
Location: `deploy/k3s/backup-controlplane.sh:29-36,45,60`
What: `identity.tgz` (server CA, SA signing key, join token) uploaded hourly to fixed blob names; the SAS at `/etc/kloudlite-git/k3s-backup.sas` and restore docs put it in argv.
Fix: SAS with `create`+`write` only; enable blob versioning/soft-delete; pass the SAS via `curl -K`.
Effort: S

### [S-31] Default ServiceAccount tokens automounted into AKS pods that never use the AKS API
Severity: low
Location: `deploy/kloudlite-git.yaml:34,285,629,779`; `deploy/kloudlite-git-web.yaml:24`
Fix: `automountServiceAccountToken: false` on each pod spec (tenant pods already do, `k8s.rs:700`).
Effort: S

### [S-32] Web pod lacks pod-level `runAsNonRoot`
Severity: low
Location: `deploy/kloudlite-git-web.yaml:24-31,112-115`
Fix: Copy the `securityContext` block from `kloudlite-git.yaml:630-634`.
Effort: S

### [S-33] Registry bearer and cross-node auth cache outlive token revocation
Severity: low
Location: `crates/registry/src/routes.rs:74` (15-min `TOKEN_TTL`), `crates/core/src/jwt.rs:149-157`, `crates/storage/src/auth.rs:41` (60 s cache)
What: Revoking a git token leaves any registry JWT valid ≤15 min and Basic auth on other nodes ≤60 s.
Fix: Accept as documented; if needed, embed the token digest in the registry JWT and re-check `owner_for_token` on write paths.
Effort: S

### [S-34] Session JWTs are unrevocable for 12 h
Severity: low
Location: `crates/core/src/jwt.rs:19,124-136`; only CLI tokens carry a `jti` (`crates/workspaces/src/api.rs:223-233`)
What: A leaked session token (S-2 makes leakage plausible) is valid until `exp`; no `jti`, no denylist.
Fix: Accept as documented, or mint sessions with a `jti` and reuse the CLI revocation row (`is_live`) — the plumbing already exists.
Effort: S

### [S-35] `/vol-agent` region scoping is trust-on-first-use on unstamped volumes
Severity: low
Location: `bins/server/src/vol_agent.rs:94-121`
What: Any region's token can claim a never-written volume name under any owner and seed its history; the real region's later push is then refused. Documented ponytail.
Fix: Stamp the region on the volume DB at `/v1` create, as the comment proposes.
Effort: M

### [S-36] Gateway single-use `jti` is per-replica
Severity: low
Location: `bins/gateway/src/tunnel.rs:36-38,60-65`
What: A 60 s ssh-session token replayed at the second replica connects. Bounded by TTL and by needing the user's ssh key.
Fix: Accept, or store spent jtis in Redis with TTL.
Effort: S

### [S-37] `kube_err` returns raw kube error strings to callers
Severity: low
Location: `crates/workspaces/src/api.rs:371`
Fix: Log it, return a fixed string.
Effort: S

### [S-38] Privileged `nix-daemon` and seed images pinned by mutable tag only
Severity: low
Location: `deploy/k3s/agent-daemonset.yaml:74,112,175`
Fix: Append `@sha256:` digests (the Dockerfiles already do this for base images).
Effort: S

### [S-39] Undocumented IP in registry rate-limit whitelist
Severity: low
Location: `deploy/kloudlite-git.yaml:932` (`4.224.42.0/32`)
Fix: Comment what it is or remove.
Effort: S

### [S-40] Pre-release crypto crates in the production lock
Severity: low
Location: `Cargo.lock`: `ssh-key 0.7.0-rc.11`, `rsa 0.10.0-rc.18` (+ `rsa 0.9.10`, RUSTSEC-2023-0071 Marvin — ignored in `deny.toml:21` with sound reasoning), `pkcs1 0.8.0-rc.4`, `argon2 0.6.0-rc.8`, `blake2 0.11.0-rc.6`; all via `russh 0.62`
Fix: Nothing now; re-run `cargo tree -d` when russh moves to released `ssh-key 0.7`.
Effort: S

---

## Verified good (do not re-audit)

**JWT / peer auth**
- HS256 pinned on every verify path; `alg: none` refused (tested); secret ≥ 32 bytes enforced; `typ` separation between `session`/`registry`/`cli`/`ssh-session` enforced by rule, with tests in both directions. Fleet mode refuses to boot without `KLOUDLITE_GIT_JWT_SECRET` (`crates/core/src/err.rs:19`).
- CLI tokens: `jti` revocation enforced on `/v1` (`api.rs:223-233`); a missing directory refuses CLI tokens rather than accepting them unrevokable. Admin routes gate on the email allowlist and accept session tokens only.
- Usernames are immutable once claimed (`directory/mod.rs:507-535`) and share one `handles` collection with team slugs, so a token's `username` claim cannot go stale or be re-taken, and a team cannot shadow a user.
- Peer secret: `secret_eq` constant-time at all three check sites (`route.rs:537`, `git/proxy.rs:56`, `api/src/lib.rs:369`); empty secret never authenticates; api refuses to boot with an empty secret.
- Public listener 404s `/api/` and strips `x-kloudlite-git-{hops,owner,peer}` (`trust_nobody`); hops bounded by `MAX_HOPS`.

**Routing / names / digests**
- Routing before auth on both listeners; `every_browse_route_is_routable`; nonexistent repos are never claimed, so anonymous scans cannot write the ownership map.
- `Digest::parse`, `valid_uuid`, OCI tag grammar, `valid_segment` on raw and decoded paths; `%2F`, `..`, `%2e%2e` refused; api-tier `split_api_path` re-encodes validated segments so reqwest's dot-segment stripping cannot re-route.
- Registry: `allow()` first in every handler; anonymous `/v2/token` yields `Anonymous`, not `Invalid`; Basic username must name the token's owner; upload `pour` counts real bytes against `max_layer` (Content-Length not trusted); PATCH `Content-Range` checked; manifest 4 MiB by layer and handler; session locks; blob lands only on digest match. GC keep-biased; only `delete_blob` and `sweep_owner` delete.

**Git protocol / SSH / merge worker**
- `want` must be reachable from refs (no fetch-by-oid of hidden objects), `have` tested by reachability; receive-pack connectivity + isolation, `Capped` reader at `max_body` over SSH, 1 GiB per-object alloc limit, gzip `take(8×max_body)`, pkt-line caps.
- SSH: publickey only, signature verified by russh before `auth_publickey`; shell/subsystem/forwarding refused; channels capped at 16; command path reuses `parse_repo_path`; no anonymous SSH.
- Merge worker: `local()`/`networked()` split holds, no networked argv in any error (tested), `GIT_TERMINAL_PROMPT=0`, identity env set.
- PR/patch author stamped by the api tier from the session, never from the body; worker endpoints require `Trusted(Some(owner))`.

**Credentials / secrets**
- 128-bit random tokens stored sha256; invite/magic-link tokens 256-bit hashed; negative cache bounded; revocation immediate on the revoking node; `upsert_user`/passkey lookup/magic-link are peer-only with body-vs-asserted identity checked.
- No committed credentials (`env.sh` git-ignored, no history); all k8s secrets are `secretKeyRef`; peer secret, agent token, Azure key, Cosmos key never formatted; no `Debug` on secret-bearing structs; Cosmos queries static.

**Workspaces / k8s**
- Every `/v1` workspace/env/volume route: `caller()` then `spec.owner` comparison (never labels); refusals are 404; listing selectors built only from the verified caller or `may_act_on`-confirmed teams; `heal_labels` re-stamps from spec.
- Gateway: token binds `ws`+`region`+`jti`; target resolved from `status.podRef` → pod IP, port fixed 22; frame sizes capped; token never logged; SA is `get` only.
- Tenant pods: no hostPath, `privileged: false`, `allowPrivilegeEscalation: false`, drop ALL, `RuntimeDefault` seccomp, `automountServiceAccountToken: false`, PSA `baseline` enforced, ephemeral-storage limits, `LimitRange`. Default-deny both directions; egress excludes RFC1918 + `169.254/16` (blocks k8s API and metadata); sshd ingress only from `kube-system` + `app=kloudlite-git-gateway` in a single AND'd peer.
- sshd: `PermitRootLogin no`, `AllowUsers kl`, `PasswordAuthentication no`; host key generated once, stored only in a per-workspace Secret, pinned by the CLI. Git seed: owner/name re-validated, branch refuses `-`/`..`, argv via env + `--`, key mounted RO, no copy on disk.
- All `btrfs`/`mount`/`losetup`/`nix`/`ssh-keygen` calls are argv, never shell; volume ids are server-minted hex; Nix attribute grammar validated twice; `validate_mount` enforced at API, StatefulSet build and before `create_dir_all`.
- api SA RBAC: spec verbs only, no `/status`; `escalate`/`impersonate`/wildcards absent; the one `bind` is `resourceNames`-scoped.
- CLI: config 0600/dir 0700, token redacted from errors, rustls with verification on.

**Web app**
- Session cookie `__Secure-authjs.session-token`, `httpOnly`, `sameSite=lax`, `secure` when `AUTH_URL` is https; production refuses to boot without `AUTH_URL`. Backend bearer never reaches the browser; peer secret used server-side only for sign-in/magic-link/passkey lookup; every `lib/*` is `server-only`; no `NEXT_PUBLIC_*`.
- Open redirects: every `next`/`redirectTo` through `safeNext` (rejects `//`, `/\`); Auth.js prefixes `baseUrl`; probed `/%2Fevil.com/repo/actions` → `307 /login`.
- XSS: the one `dangerouslySetInnerHTML` receives shiki output; hand-rolled Markdown emits React elements, no raw HTML/links; all user strings (device/workspace names, key comments, commit messages, tags) are JSX text nodes.
- CSRF: all mutations are Server Actions (Origin/Host enforced) or Auth.js POSTs with CSRF token; GET handlers are `/api/health` (204) and `/api/repos` (read, session required).
- No user-supplied URLs fetched anywhere; passkeys: challenge in `httpOnly; SameSite=Strict; 5 min` cookie, single-use, 60 s HMAC assertion with `timingSafeEqual`, counter persisted. Error pages show digest only.

**Infra / supply chain**
- Cargo: 718 packages all `registry+crates.io`, no git/path deps; `deny.toml` denies unknown sources and yanked crates; `cargo deny check advisories` ok; CI runs `rustsec/audit-check` + `cargo-deny`. `bun audit` clean (next 16.3.1, @simplewebauthn/server 13.3.2, shiki 4.4.3).
- CI: every action pinned to a full SHA; publishing jobs have `permissions: {contents: read, packages: write}`; ephemeral `GITHUB_TOKEN` for docker login; no `pull_request_target`; PR runs never push; no `curl | sh`. Dockerfile bases pinned tag+digest.
- AKS server/api/worker pods: `runAsNonRoot`, uid 1001, no priv-esc, RO root, drop ALL, memory limits, no host namespaces.
- Node hardening: nftables default-drop validated before swap; 22/6443 scoped to operator CIDRs; 80 only from Cloudflare ranges and closed-by-default; sshd keys-only, no root; unattended-upgrades; NSG never exposes 6443/10250/8472.
- Ingress: real IP from `CF-Connecting-IP` only for Cloudflare CIDRs, stale list fails safe; git LB publishes SSH only; `/v1` on the public host limited to `cli|workspaces|keys`; browse API on the peer listener only. Agent token bound to region CIDRs and rotatable; `WS_GIT_SSH_HOST` fixed.
