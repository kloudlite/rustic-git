# Read-only review: deploy/, Dockerfile, CI, gateway, kl, api auth

Scope: `deploy/**`, `.github/workflows/*`, `Dockerfile`, `bins/gateway`, `bins/kl`, JWT/auth.
Method: whole files read; every claim carries file:line. No edits made. No secret value is
reproduced below — only names and exposure paths.

Counts: **High 4, Medium 11, Low 6.**

---

## High

### H1 — The agent's tenant-namespace policy does not cover pods; agent SA can create a privileged pod in kube-system
Severity High · Category RBAC/admission gap
`deploy/k3s/agent-admission.yaml:54-95`, `deploy/k3s/agent-rbac.yaml:191-198`,
`deploy/k3s/workspace-admission.yaml:79-84`

The policy's own header claims it "pins every namespaced object the agent writes to the tenant
namespaces it makes" (`agent-admission.yaml:56-57`), but its `matchConstraints` list only
`namespaces`, `secrets` and `rolebindings` (`:69-76`). The agent also holds cluster-wide
`pods: create` (`agent-rbac.yaml:191-193`), plus `statefulsets`, `services`, `networkpolicies`,
`limitranges` — none namespace-pinned. `workspace-admission.yaml` would refuse a privileged /
hostPath pod, but its binding is scoped by `namespaceSelector` on `kloudlite-git.io/kind`
(`:79-84`), which kube-system does not carry — deliberately (`:22-24`). So a compromised agent
(or a bug in a pod builder) can `create` a privileged, `hostPath: /`, kube-system pod on any
node, mounting `kloudlite-git-jwt` and the agent Secret. That turns root-on-one-node into
root-on-the-cluster and mint-any-user's-token.

Fix: add `pods`, `statefulsets`, `services`, `networkpolicies`, `limitranges` to policy 2's
resourceRules with the same `ws-`/`wt-`/`env-` namespace test, or bind
`kloudlite-git-workspace-pod-fence` a second time with a `matchConditions` on the agent's username
and no namespaceSelector.

### H2 — Team workspaces are denied by the agent's own admission policy (`wt-` namespaces)
Severity High · Category admission correctness / availability
`deploy/k3s/agent-admission.yaml:83,85`, `crates/workspaces/src/crd.rs:752-757`,
`crates/workspaces/src/k8s.rs:974`, `bins/agent/src/controller.rs:1748,2131,2219`

`ws_namespace` returns `ws-{owner}` for a personal workspace but `wt-{owner}-{tail}` for a team
pair (`crd.rs:755-757`, asserted at `crates/workspaces/tests/crd_yaml.rs:164`). The policy admits
Namespaces starting `ws-` or `env-` only (`:83`), Secrets only in a namespace starting `ws-`
(`:85`), and RoleBindings only in `ws-`/`env-` (`:87`). With `failurePolicy: Fail` +
`validationActions: ["Deny"]`, every team workspace fails at namespace create, at
`ensure_ssh`'s `ws-ssh-{id}` Secret, and at both RoleBindings. CLAUDE.md names `wt-{owner}-…`
explicitly, so this is a manifest that never caught up with the code.

Fix: add `|| startsWith('wt-')` to all three branches (namespace, Secret namespace, RoleBinding
namespace) in `agent-admission.yaml:82-93`.

### H3 — Every owner's home is an unauthenticated NFSv3 export sharing a namespace with the internet-facing gateway, with no NetworkPolicy
Severity High · Category tenant isolation / blast radius
`deploy/k3s/zerofs.yaml:65-66,148-160`, `deploy/k3s/gateway.yaml:19-27`,
`bins/agent/src/lib.rs` (`mount_homes`, `nolock`)

ZeroFS listens on `0.0.0.0:2049` and is fronted by a ClusterIP Service; NFSv3 with AUTH_SYS and
`nolock` authenticates nothing, so anyone who can reach 2049 reads and writes `{pool}/homes/*` —
every owner's dotfiles, shell history and SSH material — for the whole region. Nothing in
`deploy/k3s/` applies a NetworkPolicy in `kloudlite-git-system`, and the one process in that
namespace that accepts unauthenticated internet connections (the gateway, `gateway.yaml:16-17`)
is a pod away from it. Tenant pods are blocked only incidentally, by
`allow_internet_egress`'s RFC-1918 exclusion (`crates/workspaces/src/k8s.rs:1222-1244`).

Fix: a default-deny NetworkPolicy in `kloudlite-git-system` plus one ingress rule admitting 2049
from `app=kloudlite-git-agent` in kube-system only — the same shape as `agent-peer.yaml`.

### H4 — gVisor is installed-but-not-enabled, and nothing requires a runtimeClass
Severity High · Category tenant isolation
`deploy/k3s/agent-daemonset.yaml:145-153`, `deploy/k3s/workspace-admission.yaml:49-70`,
`crates/workspaces/src/k8s.rs:963,1074`

`WS_RUNTIME_CLASS` is deliberately unset (the comment says so, citing audit S-29), so
`runtime_class_name` is `None` (`k8s.rs:963`, asserted at `k8s.rs:1653-1654`) and every tenant
pod runs under runc, sharing the host kernel, with hostPath mounts of the btrfs pool and
`DAC_OVERRIDE`/`SETUID` capabilities. The pod fence refuses hostNetwork/PID/IPC and privileged,
but has no validation requiring `spec.runtimeClassName == 'gvisor'`, so the boundary rests on a
single env var nobody is alerted about.

Fix: finish the rollout (`install-gvisor.sh` on both pool nodes, label, `runtimeclass.yaml`,
`WS_RUNTIME_CLASS` in the Secret) and add a fourth validation to `workspace-admission.yaml`
requiring the runtimeClass, so the fence — not an env var — is what holds.

---

## Medium

### M1 — The pod fence does not refuse capability adds
`deploy/k3s/workspace-admission.yaml:55-62`, `crates/workspaces/src/k8s.rs:667-672`
The policy exists to put back what PSA `baseline` refused (`:1-5`), but it checks only
`privileged` and `allowPrivilegeEscalation`. `k8s.rs:668-671` states the problem in the source:
"nothing above `privileged` stops us from adding SYS_ADMIN, NET_RAW, SYS_PTRACE… this fixed list
is the only thing that does". A pod builder bug or an operator with kubectl can add SYS_ADMIN in
a tenant namespace and be admitted.
Fix: add a validation asserting every container's `capabilities.add` is a subset of the seven
`hardened()` grants.

### M2 — hostPath allow-list is per-pool, not per-tenant
`deploy/k3s/workspace-admission.yaml:63-70`
Any path strictly under `/wspool-prod/` is admitted, which includes
`/wspool-prod/vol/{someone else}` and `/wspool-prod/homes/{another owner}`. The `..` check stops
traversal but not naming another tenant outright. Only the pod builders keep tenants apart.
Fix: require the hostPath to start with `/wspool-prod/vol/`, `/wspool-prod/homes/`,
`/wspool-prod/homecache/` or `/wspool-prod/attach/` *and* contain the pod's owner segment (the
`kloudlite-git.io/owner` label the builder already stamps) — CEL can compare the two.

### M3 — Spec-is-read-only policy misses CREATE, snapshots and volumereplicas
`deploy/k3s/agent-admission.yaml:29-32`, `deploy/k3s/agent-rbac.yaml:147-156`
`operations: ["UPDATE"]` and four resources. The agent holds `create` on `volumes`,
`ownerbindings`, `snapshots` and `volumereplicas` and `patch` on the last two, none of which the
policy inspects — `agent-rbac.yaml:142-146` even notes "agent-admission.yaml does not even match
`snapshots`". So the agent can author a Volume with any spec (region, quota, source) and can
rewrite a Snapshot's `parent`, i.e. graft the history chain `/v1` reads back at
`GET /v1/volumes/{name}/history`.
Fix: extend policy 1's resourceRules to `snapshots`/`volumereplicas`, restrict Snapshot `patch`
to metadata (the annotation is the only intended write), and add CREATE with a shape check on
Volume.

### M4 — `kloudlite-git-api-secrets` grants full CRUD on every Secret in a workspace namespace
`deploy/k3s/api-rbac.yaml:59-68`
The header says the API needs "exactly two Secrets" (`:51-52`), but the rule has no
`resourceNames`, and the RoleBinding puts it over the whole namespace — which also holds the
`ws-ssh-{id}` host keys the agent creates and never updates precisely because "a host key that
changed is indistinguishable from a man in the middle" (`agent-rbac.yaml:224-230`). No admission
policy constrains the API identity (`agent-admission.yaml:33-35` matches the agent's username
only).
Fix: `resourceNames: ["git-token", "user-key"]` on this ClusterRole, and drop `delete` if the
ownerReference already reclaims them.

### M5 — `kl` re-writes the workspace host key on every ssh; it is not a pin
`bins/kl/src/config.rs:95-106`, `bins/kl/src/ws.rs:18-19,27`
`pin_host_key` filters out any existing line for the id and appends whatever the API just
returned — so a changed host key is adopted silently, on every connect. The comment at
`ws.rs:18` calls this pinning. Chained with M4, an API compromise silently MITMs every workspace
SSH session with no warning at either end.
Fix: compare against the stored line and refuse (loudly, with the `kl` equivalent of ssh's
host-key-changed banner) instead of overwriting; keep the overwrite behind an explicit flag.

### M6 — `agent-peer` NetworkPolicy silently blocks the agent's metrics port
`deploy/k3s/agent-peer.yaml:9-11,27-36`, `deploy/k3s/agent-daemonset.yaml:59-62,242-245`
The file states it "restricts PORT 8444 ONLY". A NetworkPolicy with `policyTypes: [Ingress]`
selecting these pods denies *all* other ingress to them, so the `prometheus.io/scrape` on 9464
declared in the DaemonSet stops working the moment this is applied.
Fix: add a second ingress rule for 9464 from the monitoring namespace, and correct the comment —
"restricts 8444 only" is the opposite of how ingress policies compose.

### M7 — Two images pinned by mutable tag while the same files argue for digests
`deploy/k3s/agent-daemonset.yaml:163-164`, `deploy/k3s/zerofs.yaml:104-111`
`WS_GIT_INIT_IMAGE: alpine/git:2.45.2` is the init container that clones a repo with the owner's
platform key into a fresh subvolume — a moved tag runs attacker code next to that credential.
`ghcr.io/barre/zerofs:2.3.2` serves every home in the region; the immutable equivalent
(`sha-b66813e`) sits in the comment, unused. Both files pin *other* images by digest for exactly
this reason (`agent-daemonset.yaml:87-96`).
Fix: pin both by `@sha256:`.

### M8 — ZeroFS: regional SPOF on the control-plane node, no memory limit, no liveness probe
`deploy/k3s/zerofs.yaml:76,81-82,96-101,116-131`
Single replica by design (SlateDB single-writer, sound), `Recreate`, pinned to the control plane.
There is a readiness probe but no liveness probe, so a wedged-but-listening process is never
restarted, and no memory limit, so a large working set can push the control-plane node into
pressure and take the API server with it — from the pod that every home in the region depends on.
Fix: add a memory limit sized from a measured run, add a liveness probe that does more than
accept a TCP connection, and document the manual failover in `RECOVERY.md`.

### M9 — `nix-daemon` runs privileged with no resource requests or limits
`deploy/k3s/agent-daemonset.yaml:271-286`
The agent container carries a request (`:125-128`); its sibling carries none, and it is the one
that runs builds. An unbounded builder shares the node with tenants.
Fix: add requests/limits; the existing `ponytail:` note at `:276-279` about dropping to
`SYS_ADMIN` is the other half of this.

### M10 — The RBAC header table has drifted from its own rules
`deploy/k3s/agent-rbac.yaml:53,55` vs `:159-174`
The file's stated invariant is "THE TABLE BELOW IS THE ROLE" (`:3`). The table says
`*/status (all five)` while the rule grants six (`snapshots/status` included, `:159-163`), and
`*/finalizers (all five)` while the rule grants four (`:170-174`). Small, but this file's whole
review value is that the table is trusted.
Fix: correct the two counts.

### M11 — `harden-node.sh` admits every port from the pod bridge and the VNet
`deploy/k3s/harden-node.sh:59-62`
Default-drop is real (`policy drop`, `:51`) and the rest of the script is correct, but
`ip saddr $POD_CIDR accept` and `iifname "cni0" accept` admit any pod to the node's kubelet
(10250) and API (6443). The only thing keeping tenants off those is the namespace's egress
policy (`crates/workspaces/src/k8s.rs:1222-1244`) — one NetworkPolicy away from a node takeover,
where the firewall was supposed to be the second lock (`:5-7`).
Fix: replace the blanket `cni0` accept with per-port accepts for what the node actually serves
pods (nothing, for the agent's own listeners are pod-to-pod), or keep it and note in the header
that the pod network is trusted here.

---

## Low

- **L1** `deploy/../k8s.rs:1176-1192` — `allow-dns` permits egress to *every* kube-system pod on
  53/TCP+UDP, not just CoreDNS. Narrow with a `podSelector` on the CoreDNS label.
- **L2** `deploy/k3s/backup-controlplane.sh:66` — `openssl enc -aes-256-cbc` is unauthenticated;
  a truncated or tampered backup decrypts to garbage rather than failing. Key custody and the
  SAS-in-a-config-file handling (`:69-77`) are otherwise good. Consider `age` or an HMAC file.
- **L3** `bins/gateway/src/tunnel.rs:38-46` — the single-use `jti` map and both rate counters are
  per-replica, so a 60 s token can be replayed against the other replica. Already marked
  `ponytail:`; the TTL is a real mitigation. Noted, not actionable now.
- **L4** `deploy/k3s/provision-azure.sh:21-34` vs `harden-node.sh:71-72` — the NSG creates rules
  for 22/6443/8472/10250 only; the gateway's 80-from-Cloudflare rule and the AKS api tier's
  6443 access exist only as README prose (`README.md:448-458`). Two firewall layers that can
  drift silently. Fold both into the script.
- **L5** `deploy/k3s/env.example.sh` — a near-verbatim duplicate of the git-ignored `env.sh`
  (correctly untracked; the real operator CIDR is local-only). Fine as a template; flagged only
  as the one redundancy worth knowing about.
- **L6** `deploy/kloudlite-git.yaml:452-456,483-488` — the api tier reaches the k3s cluster with a
  long-lived ServiceAccount kubeconfig in the `kloudlite-git-k3s-kubeconfig` Secret, in a different
  cloud. No rotation path is documented, and the `ponytail:` note says the design replaces it.
  Add a rotation step to `deploy/k3s/README.md` until then.

## What is right (checked, not findings)

JWT handling is careful: HS256 pinned, `typ` enforced per purpose, a 32-byte secret floor, a 60 s
single-purpose SSH token bound to workspace *and* region (`crates/core/src/jwt.rs:89-193`), and
the gateway spends it only once the connect can proceed (`tunnel.rs:191-197`). The gateway holds
no user credential after the upgrade, checks region and workspace before resolving, and caps
frame *and* message size. Tenant NetworkPolicies exclude 169.254/16 and all of RFC 1918 with the
reasoning written down (`k8s.rs:1210-1221`). CI pins every action by SHA, runs `cargo audit` and
`cargo-deny`, and gates the image on the test job. `cf-sync.sh` refuses an implausible edge list
rather than writing an empty allow-list. `install-gvisor.sh` verifies out-of-band checksums.
`kl` writes its token at 0600 and scrubs it from error text.

## Architecture notes

- Two admission policies carry the isolation story, and both are narrower than their own headers
  claim (H1, M3). The headers are the design doc here, so the drift costs more than usual.
- The `wt-` bug (H2) is the shape to watch: a naming rule lives in Rust, the fence that depends on
  it lives in CEL, and only a cluster apply connects them. A test asserting every
  `ws_namespace` output satisfies the policy's prefix set would have caught it.
- Tenant isolation currently rests on three independent things (pod fence, NetworkPolicy, runc)
  and the third is off. gVisor is the one that turns the other two from "the only line" into
  defence in depth.
- ZeroFS made homes regional and made them a single unauthenticated network service. The
  single-writer reasoning is sound; the missing NetworkPolicy is what makes it a crown jewel.
- Node scripts are good: real default-drop, checksummed downloads, atomic nftables reload. Their
  weak seam is the pod bridge (M11) and the NSG/nftables split kept in prose (L4).
