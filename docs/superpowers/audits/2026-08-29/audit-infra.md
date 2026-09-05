# Infrastructure, deployment, operations and CI/CD audit — kloudlite

Scope: `deploy/kloudlite.yaml`, `deploy/kloudlite-web.yaml`, `deploy/k3s/*`, `deploy/ingress-nginx-*`, `Dockerfile`, `web/Dockerfile`, `.github/workflows/*.yml`, `tests/*.sh`, `crates/workspaces/src/crd.rs`, plus the code the manifests depend on (`bins/server/src/router/route.rs` healthz, `bins/server/src/vol_agent.rs` client_ip, `crates/core/src/log.rs`, `crates/workspaces/src/k8s.rs` PSA labels, `bins/agent/src/controller.rs` heartbeat). Read-only; no cluster was touched. Repo state: master @ 8c0b4e0, clean.

Severity counts: **high 6, medium 13, low 9** (28 findings), plus a "Verified good" list.

---

## HIGH

### [I-1] The k3s control plane is one node on SQLite, and the hourly backup has no scheduler in the repo
Severity: high
Location: `deploy/k3s/provision-azure.sh` (`CP=k3s-cp`, one VM, `CP_SIZE`), `deploy/k3s/backup-controlplane.sh`, `deploy/k3s/README.md` ("Hourly SQLite backup")
What: The CRDs are the source of truth for every workspace and environment (CLAUDE.md, `crd.rs`). That truth is `/var/lib/rancher/k3s/server/db/state.db` on a single `Standard_D2s_v5` VM with no HA datastore. `backup-controlplane.sh` is correct (`VACUUM INTO`, identity tarball, `--fail`) but nothing in the repo installs it: no systemd `.service`/`.timer`, no cron line, no instruction for creating `/etc/kloudlite/k3s-backup.sas` or the `k3s-backup` container. "Hourly" is a README claim with no artefact behind it. The script hard-codes `ACCOUNT=kloudlitegitkolomi`.
Why it matters: Lose `k3s-cp` (disk failure, botched upgrade, `az vm delete`) and every workspace/environment record is gone; the btrfs subvolumes and blob snapshots survive but nothing knows what they are. Whether a backup exists at all depends on a hand-installed timer nobody can verify from git.
Fix: Add `deploy/k3s/backup-controlplane.{service,timer}` (OnCalendar=hourly, `Persistent=true`) and an install line in the README; add a monitoring hook (a `curl` to a dead-man's-snitch / healthchecks.io URL on success) so a silent stop is noticed. Medium term: `--datastore-endpoint` to an external Postgres/etcd or embedded etcd with 3 control-plane nodes.
Effort: S (timer) / L (HA datastore)

### [I-2] `kloudlite-leader-0` is a single point of failure with no failover, and its PDB blocks node drains
Severity: high
Location: `deploy/kloudlite.yaml` StatefulSet `kloudlite-leader` (replicas: 1), PDB `kloudlite-leader` (`maxUnavailable: 0`), env `KLOUDLITE_LEADER`
What: Leadership is a pod name, chosen by config on every pod, with no election. While the leader is down: no lease renews, no cold repo can be claimed, `/own/*` answers 421 from every other pod. The PDB with `maxUnavailable: 0` means any `kubectl drain` / AKS node-image upgrade / cluster autoscaler scale-down hangs until an operator deletes the pod by hand. AKS automatic node upgrades will time out and surface as failed upgrades.
Why it matters: Node failure on the leader's node = git outage of (pod reschedule + startup) minutes with no human action; a planned AKS maintenance = a stuck drain that pages someone. `preferredDuringScheduling` anti-affinity does not prevent the leader landing beside a server pod.
Fix: Short term: document the drain runbook next to the PDB and set an AKS maintenance window; add a `priorityClassName` (system-cluster-critical-like) so the leader is rescheduled first. Medium term: the raft/replication design in `docs/superpowers/specs/2026-08-17-raft-replication-design.md`, or at minimum a `Lease`-based election so any pod may be the map writer.
Effort: S (runbook/priority) / L (election)

### [I-3] SlateDB object store, Mongo/Cosmos and blob snapshots have no documented backup or retention policy
Severity: high
Location: `deploy/kloudlite.yaml` (`KLOUDLITE_S3_URL: az://kloudlite`, `kloudlite-mongo`, `kloudlite-cosmos`), `deploy/k3s/README.md`, root `README.md`
What: Every git repo, every registry manifest/tag, every pull request (Mongo) and every workspace snapshot (Azure blob `wslayers*`) lives in Azure resources that are never named as backed up. No mention anywhere of Azure Blob soft-delete, versioning, point-in-time restore, geo-redundancy, or Cosmos continuous backup. The GC sweep (`crates/registry/src/gc.rs`) and an explicit `DELETE /v2/.../blobs` are destructive against the primary copy. An operator with the storage key (which sits in a plain Secret on 6 pod specs) can `az storage container delete kloudlite` and there is no stated way back.
Why it matters: SlateDB is a log-structured store; a bug that writes a bad manifest or a mistaken container delete is unrecoverable without object-store-level versioning. This is the whole product's data.
Fix: Enable blob soft-delete (container + blob, >=14d) and blob versioning on the `kloudlite` and `wslayers*` containers; enable Cosmos continuous backup (7d) for the Mongo-API account; record the settings and a restore drill in `deploy/README` or `docs/`. These are Azure-side toggles, not code.
Effort: S

### [I-4] No metrics, no alerting, no structured logs — observability is `kubectl logs` and probes
Severity: high
Location: `crates/core/src/log.rs` (text `fmt()` to stderr, `EnvFilter`), no `/metrics` route anywhere (`grep prometheus|/metrics` over `crates bins` is empty), no ServiceMonitor/alert rules in `deploy/`
What: The only health signals are k8s probes. There is no counter for fenced-handle errors (`Closed error: detected newer DB client`, the invariant violation CLAUDE.md calls out), claim failures, 421s, merge-worker lane starvation, OOMKills, 413s from the body cap, GC sweep aborts, or push failures at the registry. Logs are unstructured text so they cannot be queried on a field. Nothing pages anyone: a leader crash loop, a stuck drain, or the backup timer dying is discovered by a user.
Why it matters: Several incidents in the yaml comments (leader crash loop for an hour, OOMKill on a 1.5GB push, registry redirect loop for 2 minutes) were found by symptoms, not signals. That pattern will repeat.
Fix: `metrics-exporter-prometheus` (or `axum-prometheus`) on the peer listener with a dozen counters (fence detected, claim granted/heldby/failed, 421, 413, gc sweep result, merge outcome, vol-agent push result); a `ServiceMonitor` + 5 alert rules (leader not Ready >2m, any pod restart >3/h, srv CrashLoop, worker heartbeat probe failing, backup snitch missing). Switch `fmt()` to `.json()` under an env flag so Azure Log Analytics can index fields.
Effort: M

### [I-5] `KLOUDLITE_AGENT_SOURCES` IP allow-list is enforced from `X-Real-IP`/`X-Forwarded-For`, which is only trustworthy once the origin lock is applied — and that lock is a manual `kubectl patch` outside the manifests
Severity: high
Location: `bins/server/src/vol_agent.rs` `client_ip()` (reads `x-real-ip`, then first hop of `x-forwarded-for`), `deploy/ingress-nginx-config.yaml` (`use-forwarded-headers: "true"`), `deploy/ingress-nginx-origin-lock.md` (manual `loadBalancerSourceRanges` patch), `deploy/kloudlite.yaml` registry Ingress comment ("dig cr.khost.dev resolves to Cloudflare today")
What: The vol-agent surface (`/vol-agent/*` on `cr.khost.dev`, i.e. every workspace push/ref-move) is gated by a region token AND a source-IP check. `use-forwarded-headers: true` makes ingress-nginx honour client-supplied `X-Forwarded-For`; `X-Real-IP` is set from the realip result, which is only correct when the connection arrives from a Cloudflare range. Any client that reaches the LoadBalancer IP directly (the origin lock is a doc, not a manifest, and there is no way to see from git whether it is applied) can supply `X-Forwarded-For: 40.80.82.158` and the header lands in the list the server consults. The same gap defeats the per-IP rate limits and the `limit-whitelist`.
Why it matters: A leaked agent token becomes usable from anywhere, which is exactly what the comment on `KLOUDLITE_AGENT_SOURCES` says cannot happen. The Cloudflare edge is the security boundary but nothing in the repo proves it is closed.
Fix: (1) Put `loadBalancerSourceRanges` in a committed manifest for `ingress-nginx-controller` Service (a patch file applied with the rest), not a `.md`. (2) In `client_ip`, read only `X-Real-IP` (ingress-nginx always sets it; never trust `X-Forwarded-For`). (3) Note in the Ingress that `use-forwarded-headers` is safe ONLY with the source-range lock and cross-reference the two files.
Effort: S

### [I-6] Every Azure credential is a long-lived account key in a Secret, shared by all tiers, with no rotation story
Severity: high
Location: `deploy/kloudlite.yaml` (`kloudlite-storage` account/key on leader, srv, api, worker; `kloudlite-mongo`; `kloudlite-cosmos` incl. `vol-agent-token`; `kloudlite-jwt`; `kloudlite-peer`), `deploy/k3s/README.md` ("agent Secret … created by hand"), `deploy/k3s/gateway.yaml` (jwt copied cluster to cluster)
What: The storage account KEY (full read/write/delete on every blob), the Cosmos key, and the JWT signing secret are static Secrets created by hand. The api tier — the one that "cannot open a repository for writing because none of that code is linked" — still holds the same storage key as the servers. Only the region agent token has a rotation script; nothing covers the storage key, Cosmos key, peer secret or JWT (a JWT rotation requires simultaneous update in two clusters). No SealedSecrets/ESO/Key Vault CSI, so the values exist only in cluster state and someone's shell history.
Why it matters: Blast radius of any pod compromise is the whole storage account; rotation being undocumented means it will not happen after an incident; loss of the cluster loses the secrets (see I-9).
Fix: Workload Identity + RBAC on the storage account (`Storage Blob Data Contributor` for srv/worker, `Data Reader` for api) so no key exists; until then, Key Vault CSI driver or External Secrets so the values are recoverable and rotatable; add a `rotate-jwt.sh` that patches both clusters in one step, mirroring `rotate-agent-token.sh`.
Effort: M

---

## MEDIUM

### [I-7] CI publishes `:latest` and `:<sha>` images from commits whose test job failed
Severity: medium
Location: `.github/workflows/image.yml` (`build` job: "NOT needs: test"), `deploy/k3s/dev-push.sh`
What: The comment is right that SHA tags cannot be deployed by accident, but `:latest` is moved on every push regardless of test outcome, and `imagePullPolicy: IfNotPresent` plus SHA pins are the only defence. Nothing marks a SHA as "tests passed"; the operator repinning has to open the Actions run and read the test job. `web.yml` does gate (`needs: check`), so the two workflows disagree.
Why it matters: A repin done from the Packages page or `git log` picks a red commit with no signal.
Fix: Keep the parallel build but add a third job `needs: [test, build]` that `docker buildx imagetools create -t ghcr.io/kloudlite/kloudlite:tested-${sha}` (a tag copy, seconds) — and tell the README to pin `tested-` tags. Drop `:latest` entirely; nothing in `deploy/` uses it.
Effort: S

### [I-8] The two-image drift problem is real and only enforced by memory
Severity: medium
Location: `deploy/kloudlite.yaml` (server/api/worker pinned to `d520413`, 2026-08-28), `deploy/kloudlite-web.yaml` (web pinned to `5046c27`, 2026-08-29), `deploy/k3s/agent-daemonset.yaml` (agent `0874250`), `deploy/k3s/gateway.yaml` (gateway `59a6bd0`)
What: Four images, four independently pinned SHAs across three files. `web.yml` runs only on `web/**` paths, so a server SHA frequently has no web image and vice versa. Verified: all four SHAs are real master commits and `59a6bd0` post-dates the gateway target (the "this SHA is a placeholder and will not pull" comment in `gateway.yaml` is stale). 13 non-web commits exist since the server pin — the server tier is behind the agent and gateway, which were built from the same Dockerfile at a later SHA, so agent ↔ vol-agent protocol compatibility is currently untested-by-construction.
Why it matters: The agent (`0874250`) talks to a server (`d520413`) built 13 commits earlier; a wire change in between is silently live. The stale "placeholder" comment will make the next operator re-pin unnecessarily or distrust a valid pin.
Fix: A `deploy/pins.env` (`SERVER_SHA=…`, `WEB_SHA=…`) plus a 10-line `deploy/render.sh` (`envsubst`) so one file declares versions; a CI job `deploy-check` that fails if a pinned SHA has no corresponding package tag. Delete the stale placeholder comment in `gateway.yaml`.
Effort: S

### [I-9] There is no from-scratch recovery document; the personal runbook is git-ignored
Severity: medium
Location: `.gitignore` (`/RUNBOOK.local.md` — "Personal ops runbook with cluster-specific commands; not for the repo"), `deploy/k3s/README.md` (k3s only), no `deploy/README.md` for AKS
What: To rebuild the AKS side an operator must know to create 10 Secrets (`kloudlite-storage`, `-jwt`, `-peer`, `-hostkey` (how is the host key generated? — `ssh-keygen` per Dockerfile comment, format unstated), `-mongo`, `-redis`, `-cosmos` with 3 keys, `-k3s-kubeconfig` (a SA token for `kloudlite-api` in the other cluster — no `kubectl create token` line anywhere), `-web` with up to 7 keys, `-mail`), install ingress-nginx + cert-manager + a `letsencrypt` ClusterIssuer, apply the ConfigMap merge, patch `loadBalancerSourceRanges`, configure Redis `volatile-lru`, and set Cloudflare DNS/SSL modes for three hostnames in two modes (Flexible vs Full strict). That list is reconstructed from comments across five files; none of it is a procedure.
Why it matters: Recovery time after a cluster loss is bounded by one person's memory. The k3s README shows the team CAN write this; the AKS half is missing.
Fix: `deploy/README.md`: ordered apply list, every Secret with its keys and how each value is minted, the two Cloudflare zone settings, cert-manager/ingress prerequisites, and a "verify" block (`kubectl get pods`, `git ls-remote`, `docker pull`). Untrack-proof it by moving the non-secret parts of `RUNBOOK.local.md` in.
Effort: M

### [I-10] `harden-node.sh` is not idempotent for `API_CLIENTS` and can lock the operator out on an interface rename
Severity: medium
Location: `deploy/k3s/harden-node.sh`
What: (a) `IFACE` is detected from the default route at run time; on Azure the NIC can come back as a different name after a resize (`eth0` vs `enP…`), leaving the SSH accept rule bound to a stale name — with `policy drop`, that is a lockout requiring serial console. (b) `API_CLIENTS` defaults to empty, so a re-run for `CF_CIDRS` that forgets `API_CLIENTS` silently drops the AKS api tier's access to 6443 (the README's CF_CIDRS example omits it). (c) `nft delete table` then `nft -f` is a two-step replace; a failure between them leaves the node with NO firewall table (open, not closed). (d) `systemctl reload ssh` — on Ubuntu 24.04 the unit is `ssh.service` but with socket activation `reload` may not re-read `sshd_config.d`; `sshd -t` passes regardless.
Why it matters: Each is a "run it again and the node is silently different" — the definition of drift for a script the README says to re-run after any CIDR change.
Fix: Use `iifname != {"lo","cni0","flannel.1"}` semantics or match on the public IP (`ip daddr $PUBIP`) instead of the interface name; make `API_CLIENTS` required (`:?`) like `ADMIN_CIDR`; write the new ruleset with `nft -f` using `flush table inet node` inside the file (atomic replace in one transaction); `systemctl restart ssh` after `sshd -t`.
Effort: S

### [I-11] Cloudflare IP list is hand-copied in three places and the NSG duplicates nftables
Severity: medium
Location: `deploy/k3s/cloudflare-ips-v4.txt`, `deploy/ingress-nginx-config.yaml` (`proxy-real-ip-cidr`), `deploy/ingress-nginx-origin-lock.md`, `deploy/k3s/README.md` step 5 (NSG rule by hand), `harden-node.sh` (nftables)
What: The same 15 CIDRs live in a text file, an nginx ConfigMap literal, and a doc; the NSG rule for the gateway is an `az` command in the README, not a script; `provision-azure.sh` does not create it. IPv6 ranges are absent everywhere while the origin-lock doc says to include `ips-v6`. When Cloudflare changes a range (last change 2024), the three copies drift independently and the failure is per-file: stale nginx list = rate-limit collapse for one edge; stale NSG = that edge blocked; stale nftables = same.
Why it matters: "Fails safe" is true per copy but the operator has to remember all three plus the NSG.
Fix: One source (`cloudflare-ips-v4.txt`), one `deploy/k3s/cf-sync.sh` that renders the ConfigMap literal, the NSG rule (`az network nsg rule create/update`), and the `loadBalancerSourceRanges` patch from it; run it in CI as a diff check (`curl https://www.cloudflare.com/ips-v4 | diff - cloudflare-ips-v4.txt`). Keep nftables as the second lock (the reasoning in the script is sound) but feed it from the same file.
Effort: S

### [I-12] Cloudflare "Flexible" SSL means plaintext from the edge to the origin for the app AND the workspace gateway, and credentials ride it
Severity: medium
Location: `deploy/kloudlite-web.yaml` Ingress comment ("Cloudflare's SSL mode here is Flexible"), `deploy/k3s/gateway.yaml` ("TLS terminates at the edge (the hostname's SSL mode is Flexible)"), `deploy/kloudlite.yaml` registry Ingress (`ssl-redirect: "false"`, has a cert but is proxied and fetched over HTTP)
What: Git Basic auth, session cookies, registry bearer tokens and workspace SSH session tokens all traverse Cloudflare → Azure public IP over HTTP. The registry ingress even has a valid cert-manager certificate but the edge is not using it. The k3s README (step 2) says "SSL/TLS mode Full (strict)" for the zone while `gateway.yaml` says Flexible — the two documents disagree about the current state of the same zone.
Why it matters: Anyone on the path (Azure network, a BGP hijack of the LB IP) reads credentials. And the documentation cannot both be right.
Fix: Flip the zone to Full (strict); the registry ingress already has a real cert and the gateway binary already serves 443 with `GATEWAY_TLS_DIR` (Origin CA cert per README step 3). For `dev.kloudlite.io`, add a cert-manager TLS block to the web Ingress. Then set `ssl-redirect: "true"` on all three and delete the Flexible comments. Reconcile the README/gateway.yaml claim.
Effort: S

### [I-13] Rollout strategy for the server StatefulSet is the default and the readiness probe does not prove the pod can serve its repos
Severity: medium
Location: `deploy/kloudlite.yaml` `kloudlite-srv` (no `updateStrategy`, no `podManagementPolicy`), `healthz` in `bins/server/src/router/route.rs`
What: `/healthz` returns 200 when the object store answered recently; it does not check that the ownership map/leader is reachable, that the peer listener is up, or that the pod has finished re-claiming its ordinal's repos. On a roll, `srv-2` is killed (15s preStop + release), then the new `srv-2` is Ready as soon as the blob store pings, while its repos are being force-claimed by survivors or are ownerless. The default `RollingUpdate` with `partition: 0` proceeds by ordinal; combined with `maxUnavailable: 1` PDB this is fine for drains but the CLAUDE.md note "the first registry request to a moved image can 500 once (known fenced-handle gap)" is a correctness gap the probe cannot see.
Why it matters: Readiness gating DNS membership is the load-bearing use here (comment says so); a Ready pod that cannot yet reach the leader gets traffic and answers 5xx.
Fix: Make `/healthz` on the public listener also require "leader reachable within the last lease TTL" (a cached bool from the renew loop) so a pod whose claims are failing goes un-Ready; keep the peer `/healthz` as-is. Consider `updateStrategy.rollingUpdate.partition` for canary rolls on one ordinal. Track the fenced-handle gap as an issue with a repro.
Effort: M

### [I-14] The api Deployment and the worker have no PDB, and the web has no PDB or anti-affinity
Severity: medium
Location: `deploy/kloudlite.yaml` (`kloudlite-api` replicas 2, `kloudlite-worker` replicas 1), `deploy/kloudlite-web.yaml` (replicas 2)
What: A drain may evict both api pods or both web pods together (no PDB, no anti-affinity, default `RollingUpdate` 25%/25% is only for updates). The worker is a singleton with no PDB — a drain kills merge processing until reschedule; `announce_stranded_merges` covers it, but there is no `topologySpreadConstraints` on anything but the git StatefulSets.
Why it matters: Planned maintenance becomes an outage of the browse API/web UI for the reschedule window.
Fix: `PodDisruptionBudget minAvailable: 1` for api and web; `podAntiAffinity preferred` on both; leave the worker (it is idempotent by design) but note it.
Effort: S

### [I-15] Agent DaemonSet: `privileged`, `hostPath /dev`, `:latest`-free but `nixos/nix:2.24.10` unpinned by digest, Nix store seeded from an unverifiable image
Severity: medium
Location: `deploy/k3s/agent-daemonset.yaml` (`seed-store` init container, `nix-daemon` sidecar), `deploy/k3s/nix-conf.yaml` (`trusted-users = root`)
What: The agent being privileged is justified in the comments (btrfs, loop mounts). But the two `nixos/nix:2.24.10` containers are tag-pinned only (mutable), also privileged, and the first one copies its entire `/nix` onto the host once — after which every workspace's toolchain descends from whatever that tag resolved to that day. `trusted-users = root` plus `NIX_REMOTE=daemon` from a privileged agent means the daemon accepts any substituter/option override from the agent process. `install-gvisor.sh` fetches `release/latest` (moving) though it verifies sha512 from the same origin.
Why it matters: Supply-chain: a compromised `nixos/nix` tag or gvisor release becomes root on every pool node. Reproducibility: two nodes provisioned on different days have different host stores despite the comment "so the two nodes cannot drift".
Fix: Pin `nixos/nix@sha256:…` and gVisor to a dated release URL; drop `privileged` from `nix-daemon` (it needs `SYS_ADMIN` for the sandbox at most) and from the init container entirely (it only copies files).
Effort: S

### [I-16] Agent ClusterRole grants exceed what the controller calls, and `bind` + `rolebindings` make it a privilege-escalation path
Severity: medium
Location: `deploy/k3s/agent-rbac.yaml`
Verified usage (grep of `Api<…>` in `bins/agent`, `crates/workspaces`): Namespace, Pod, Service, PVC, PV, LimitRange, NetworkPolicy, StatefulSet, Deployment, Secret (get/create), RoleBinding, Node (get, `bins/agent/src/lib.rs:129`), OwnerBinding + the CRDs.
Not seen used: `events create/patch` (no `Api<Event>`/recorder in the agent or workspaces crate — only the test `Recorder`), `nodes list/watch` (only `get`), `services` `delete` beyond reconcile (likely used — fine), `statefulsets`/`deployments` `watch` (controller watches CRDs, not apps).
Concerning: `secrets get/create` cluster-wide lets the agent read `kloudlite-jwt` and `kloudlite-agent` in `kube-system` (its own creds, acknowledged) and any workspace `user-key`/git token Secret; `rolebindings *` + `bind` on `kloudlite-api-secrets` (secrets create/update/patch/delete) means the agent can grant secret-write anywhere it can create a RoleBinding — every namespace.
Why it matters: The agent runs privileged on a tenant-sharing node; a tenant escape to the agent is cluster-admin-equivalent via this role. The comment acknowledges the ceiling (`ponytail:` marker) and proposes a ValidatingAdmissionPolicy; nothing exists yet.
Fix: Drop `events`, `nodes list/watch`, apps `watch`. Add the ValidatingAdmissionPolicy the comment names (deny `secrets` get outside `ws-*`/`env-*` namespaces for this SA; deny RoleBinding create outside those prefixes). k8s ≥1.30 has VAP GA; k3s v1.33 qualifies.
Effort: M

### [I-17] `kube-system` for the agent and gateway means PSA `privileged`, and the gateway (internet-facing, hostPort 80) gets it too
Severity: medium
Location: `deploy/k3s/gateway.yaml` (namespace `kube-system`, justification: NetworkPolicy peer selector), `deploy/k3s/agent-daemonset.yaml`
What: The gateway is the one process that accepts unauthenticated internet connections (before token check). Placing it in `kube-system` exempts it from PSA and puts it beside the agent's credential Secret (`WS_REGION` is read from `kloudlite-agent` — the same Secret that holds `WS_AGENT_TOKEN` and `AZURE_KEY`; a `secretKeyRef` needs no RBAC but a compromise of the pod's SA (`get pods` cluster-wide) is one step from a Secret read if any future rule is added). Tenant namespaces are `baseline` enforce / `restricted` warn+audit (`k8s.rs:130-133`) — good — but `baseline` still allows hostPorts and unconfined seccomp for tenant pods unless the controller sets them.
Why it matters: Namespace placement chosen for a NetworkPolicy selector is the wrong lever; a `namespaceSelector` on the policy works equally.
Fix: A `kloudlite-system` namespace with `pod-security.kubernetes.io/enforce: baseline` for the gateway (and `privileged` only where the agent lives); the workspace NetworkPolicy selects `namespaceSelector: {kubernetes.io/metadata.name: kloudlite-system}` + `podSelector app=kloudlite-gateway`. Put `WS_REGION` in its own ConfigMap rather than reading it from the credential Secret. Have the controller set `seccompProfile: RuntimeDefault` on tenant pods so `restricted` audit stops warning.
Effort: S

### [I-18] Worker liveness counts heartbeat files with `-mmin -30`: a wedged lane takes up to 33 minutes to restart, and an emptyDir reschedule resets nothing it should
Severity: medium
Location: `deploy/kloudlite.yaml` `kloudlite-worker` livenessProbe
What: The probe is thoughtful (all lanes must be alive) but its window is 30 minutes + 3×60s. A lane that deadlocks on the first job after start also passes for 30 minutes since files are written once per iteration and the first write happens before the job. There is no readiness probe (fine, no listener) and no metric for "job age", so a stuck merge is invisible until the restart.
Why it matters: A user's merge button spins for half an hour before the system self-heals; no alert precedes the restart.
Fix: Write the heartbeat *around* the job (before and after) and lower the window to 2× the client timeout (say `-mmin -5`) — the comment's "sixteen nudges at 60s" case would need the job itself to be silent for 16 minutes, which is already a bug; export lane age as a metric (see I-4).
Effort: S

### [I-19] Registry rate-limit whitelist and `KLOUDLITE_AGENT_SOURCES` disagree; both are hand-maintained node IPs
Severity: medium
Location: `deploy/kloudlite.yaml` (`limit-whitelist: 40.80.82.158/32,20.219.22.61/32,4.224.42.0/32` vs `KLOUDLITE_AGENT_SOURCES: centralindia-k3s=40.80.82.158/32,20.219.22.61/32`)
What: A third IP (`4.224.42.0/32`, undocumented — the build VM? an old node?) is exempt from rate limiting but not an allowed agent source. Node public IPs are Standard-SKU and stable, but a `provision-azure.sh` re-run after a VM delete gives new ones, and nothing in the repo derives these literals from the VM inventory.
Why it matters: Adding a node = editing two literals in a 959-line file in two places (leader AND srv copies, per the `ponytail:` note) plus the Ingress annotation; missing one = 401 on every push from that node (exactly the incident referenced "seen 27 Aug as ref move: 401" for the region mismatch case).
Fix: A single `deploy/regions.env` rendered into both by the same `render.sh` as I-8; explain or remove `4.224.42.0/32`.
Effort: S

---

## LOW

### [I-20] `image.yml` runs `cargo test` without `--locked` and CI builds have no timeout
Severity: low
Location: `.github/workflows/image.yml`
What: `cargo clippy`/`cargo test` may rewrite `Cargo.lock` on a stale lockfile and pass; the Dockerfile uses `--locked` so the image build would then fail differently from the test job. No `timeout-minutes` on either job (default 6h); a hung test burns runner minutes silently. `kl.yml` uses `--locked` correctly.
Fix: `cargo test --locked`, `cargo clippy --locked`, `timeout-minutes: 30` on each job.
Effort: S

### [I-21] `web.yml` PR trigger runs the check job but `image.yml` has no PR trigger at all
Severity: low
Location: `.github/workflows/image.yml` (`on: push: branches: [master]` only)
What: Rust clippy/tests only run after merge to master. A PR workflow is the cheap place to catch a red commit before it becomes a `:latest` image (I-7).
Fix: Add `pull_request:` to the `test` job's triggers (build job stays push-only via `if: github.event_name != 'pull_request'`, as web.yml does).
Effort: S

### [I-22] `rustsec/audit-check` and `cargo-deny` run only on master push, and `web` has no dependency audit
Severity: low
Location: `.github/workflows/image.yml`, `.github/workflows/web.yml`
What: Advisories arrive without a commit; a scheduled run is what catches them. `bun audit` (or `osv-scanner`) is absent for the web tree.
Fix: `schedule: cron: "0 6 * * 1"` on a small `audit.yml` running `cargo deny check advisories` and `bun audit`.
Effort: S

### [I-23] Actions are SHA-pinned (good) but there is no Dependabot/Renovate config to move them
Severity: low
Location: `.github/` (no `dependabot.yml`), `Dockerfile` (digest pins with "re-resolve by hand" comment)
What: Every pin will rot silently; the Dockerfile comment asks the human to `imagetools inspect`.
Fix: `.github/dependabot.yml` with `github-actions`, `docker`, `cargo`, `npm` (directory `/web`) ecosystems, weekly.
Effort: S

### [I-24] `kl.yml` release has no provenance/signing and downloads `cross` from crates.io at run time
Severity: low
Location: `.github/workflows/kl.yml`
What: `cargo install cross --locked` pulls an unpinned version; release assets are sha256-listed but not signed; `generate_release_notes: true` on a tag push is fine. `install.sh` verifies against `sha256sums` served from the same release, so the checksum proves integrity, not origin.
Fix: `cargo install cross --version 0.2.5 --locked`; `actions/attest-build-provenance` (free for public repos) or cosign keyless on the four binaries.
Effort: S

### [I-25] `kubectl apply -f deploy/kloudlite.yaml` applies both StatefulSets in one shot; the leader has no explicit roll order
Severity: low
Location: `deploy/kloudlite.yaml`, CLAUDE.md "Deploying"
What: Splitting the leader out was meant to let the two halves roll independently, but a single `apply` of the file rolls the leader AND begins the srv roll simultaneously — the leader restart (≈30s) overlaps the first srv ordinal's re-claim, which is the window in which claims fail. The yaml comments do not say "roll the leader first, wait, then srv".
Fix: Two files (`leader.yaml`, `srv.yaml`) or a `deploy/roll.sh` that applies the leader, `rollout status`, then the rest. Document the order in CLAUDE.md.
Effort: S

### [I-26] `ws_e2e.sh` is documented as "authored without ever being run locally" and is not wired to any CI
Severity: low
Location: `tests/ws_e2e.sh` header, `.github/workflows/*` (no reference), `deploy/k3s/README.md` ("what tests/ws_e2e.sh's seeded phase proves in CI" — but no CI runs it)
What: 822 lines, 40+ steps (workspace create/push/clone/restore, packages, gateway ssh, NetworkPolicy, environment, stop-pushes), exit 77 on 8 prerequisites — thorough, and the only end-to-end proof of the workspaces control plane. The README claims CI proves it; nothing does. `registry_e2e.sh` is likewise not in CI (docker is available on ubuntu-latest, so the docker half could run).
Fix: Add `registry_e2e.sh` to `image.yml`'s test job behind `cargo run -- serve` with `mem://` (needs a token-mint step; treat 77 as failure in CI). For `ws_e2e.sh`, a self-hosted-runner job on the build VM (which already exists) with `workflow_dispatch` + weekly schedule; fix the README wording until then.
Effort: M

### [I-27] `dev-push.sh` pushes `dev-*` tags to the production GHCR namespace and rolls the DaemonSet without a pin edit
Severity: low
Location: `deploy/k3s/dev-push.sh`
What: `kubectl set image` on the live DaemonSet leaves `agent-daemonset.yaml` claiming a SHA that is not running; `git status` shows nothing. `--exclude web` in rsync is fine; `sudo docker` on the build VM means root builds. The `-dirty` suffix protects tag confusion, not manifest drift.
Fix: Print a loud "MANIFEST NOW DRIFTED" and the exact `kubectl apply -f agent-daemonset.yaml` to reconcile; or write the dev tag into a `.local/` override file the README mentions.
Effort: S

### [I-28] `format-pool.sh` and `install-gvisor.sh` are correct but the pool has no monitoring for btrfs free space or `subvolume` count
Severity: low
Location: `deploy/k3s/format-pool.sh` (`defaults,noatime`, no `space_cache=v2`/`discard=async`), agent DaemonSet (no disk-pressure signal)
What: A full btrfs pool fails writes in odd ways (ENOSPC on metadata while `df` shows space); nothing exports `btrfs fi usage`. Premium_LRS with no `discard` slowly loses reclaim on snapshot deletes.
Fix: Mount with `noatime,discard=async,space_cache=v2`; node-exporter textfile collector or an agent gauge for pool free bytes + subvolume count; alert at 80%.
Effort: S

---

## Verified good

- **Image pins are all real master commits** (`d520413`, `5046c27`, `0874250`, `59a6bd0` — checked with `git log`), the gateway pin post-dates the `gateway` build target, and no manifest references `:latest`. `imagePullPolicy: IfNotPresent` on immutable tags is the right call and is explained.
- **Dockerfiles**: base images pinned tag+digest; cargo-chef split with the profile-mismatch trap documented; three targets from one compile; non-root uid 1001 everywhere except the agent (justified); gateway `setcap` reasoning is correct (file cap + bounding set). `web/Dockerfile` pins bun and node by digest, hoisted linker with the reason.
- **Every GitHub Action is SHA-pinned** with a version comment; `GITHUB_TOKEN` is the only CI secret, with least-privilege `permissions:` per job (`packages: write` only on image jobs, `contents: write` only on release). No long-lived PAT in CI. Turbo cache keyed on the lockfile with the correct reasoning.
- **Server-tier pod security**: `runAsNonRoot`, `readOnlyRootFilesystem`, `drop: ALL`, `allowPrivilegeEscalation: false`, `fsGroup` for the host key, emptyDir with `sizeLimit` — on all five server-cluster workloads. Memory limits with measured justification; deliberate absence of CPU limits explained correctly (throttling vs. OOM).
- **Probes**: startup/readiness/liveness split with the real incident that motivated it; the leader's shorter startup budget (60 vs 120) is reasoned; `publishNotReadyAddresses: true` on the headless Service with the lease-TTL reasoning; 15s preStop sized from DNS + membership TTLs; `terminationGracePeriodSeconds` accounts for it.
- **PDBs on both StatefulSets** with the exact semantics wanted (`maxUnavailable: 1` for srv, `0` for the leader — the latter is a deliberate "drain must be watched" choice, flagged above only for its operational cost).
- **Registry ingress**: separate hostname, `proxy-body-size: 0`, `proxy-request-buffering: off`, 600s timeouts, per-IP rate limit with a whitelist; the `/v1` path publishes only the three CLI prefixes and the comment records the mistake it fixes. `kloudlite-api` Service left ClusterIP with an intentionally invalid placeholder so an apply fails closed.
- **Redis eviction policy** requirement (`volatile-lru`) documented at the point of use with the failure mode under `allkeys-lru`.
- **k3s scripts**: `provision-azure.sh` NSG-before-NIC ordering, guarded creates, `--nsg ""` reasoning; `format-pool.sh` refuses an existing filesystem and mounts by UUID; `harden-node.sh` validates the ruleset (`nft -c`) before touching the live table and deletes only its own table (does not flush k3s's iptables-nft); `backup-controlplane.sh` uses `VACUUM INTO`, bundles TLS/token, `--fail`s uploads, and has a correct restore procedure including the WAL/SHM removal; `install-gvisor.sh` verifies sha512 and edits the containerd template not `config.toml`.
- **CRDs**: five cluster-scoped kinds with status subresources; `selectableFields` on `.status.nodeName` for parents (controller-established fact) and `.spec.nodeName` for children; `SnapshotRequest` deliberately non-selectable; RBAC split spec-writer (api) / status-writer (agent) is real (`/status` absent from the api role, `snapshotrequests/status` absent too); api's `secrets` grant is per-namespace via controller-created RoleBinding + `bind` on one named ClusterRole. Gateway SA is `get`-only on two kinds.
- **StorageClass**: `no-provisioner`, `WaitForFirstConsumer`, `Retain` — each with the correct reason.
- **Tenant namespaces** get PSA `enforce: baseline`, `warn/audit: restricted` (`crates/workspaces/src/k8s.rs:130-133`, unit-tested).
- **e2e scripts**: `exit 77` skip convention implemented and explained in both; `ws_e2e.sh` guards against `grep -c` under `pipefail`, uses one trap, refuses to run beside the DaemonSet, and reuses one Cosmos DB to stop the leak it documents.
- **`rotate-agent-token.sh`** does both halves (mint + patch + restart) in one step, with the reason.
- **`env.sh` is git-ignored** and contains no secrets (verified: parameters only).
