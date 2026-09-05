# Deploy and Security Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the DEPLOY/SECURITY findings of the 2026-09-02 review: make the two admission policies as wide as their own headers claim (agent may not create a privileged kube-system pod; team `wt-` namespaces stop being denied), put the unauthenticated NFS home export behind a NetworkPolicy, make the pod fence — not an env var — require gVisor, bound capabilities and hostPath subtrees, narrow the api's Secret grant, make `kl`'s host key an actual pin, and fix the smaller manifest/script drifts.

**Architecture:** Two ValidatingAdmissionPolicies carry the isolation story (`agent-admission.yaml` matches on the agent's identity, `workspace-admission.yaml` matches on tenant namespaces). Every naming rule those policies depend on lives in Rust (`crd::ws_namespace`, `k8s::hardened`, `k8s::host_dir` paths), and only a cluster apply connects the two halves — which is exactly how the `wt-` bug shipped. So the first task is a Rust test that ties `ws_namespace` to the policy's prefix set by parsing the YAML, and the manifest fixes land against it. Manifest changes that apply together are one commit so the cluster is never half-fenced; changes that need a measured number (ZeroFS memory) or that change runtime behaviour (`kl` pinning) are their own tasks.

**Tech Stack:** Kubernetes `admissionregistration.k8s.io/v1` (CEL), `networking.k8s.io/v1` NetworkPolicy, RBAC v1, Rust 2021 (`crates/workspaces`, `bins/kl`), nftables/Azure CLI shell scripts.

**Spec:** docs/superpowers/reviews/2026-09-02-codebase-review.md (details: docs/superpowers/reviews/2026-09-02-details/deploy-security.md)

## Global Constraints
- Namespace prefixes are exactly three: `ws-` (personal), `wt-` (team pair), `env-` (environment) — `crates/workspaces/src/crd.rs:752-757`.
- The seven allowed capabilities are exactly `CHOWN, DAC_OVERRIDE, FOWNER, SETGID, SETUID, NET_BIND_SERVICE, SYS_CHROOT` — `crates/workspaces/src/k8s.rs:676-679`.
- The pool root is `/wspool-prod`, which must stay in step with `WS_POOL` in `deploy/k3s/agent-daemonset.yaml`.
- The owner label is `kloudlite.io/owner`, stamped on every tenant pod and on the env StatefulSet pod template (`k8s.rs:30,72-77,1030-1032,1101-1104`) — a view label, used here only to scope a hostPath, never to authorize.
- The agent's identity is `system:serviceaccount:kube-system:kloudlite-agent`.
- Manifest verification is `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f <file>` run from the repo root; never apply without the dry-run passing first.
- Rust CI gate: `cargo clippy --workspace --all-targets --locked -- -D warnings` plus `cargo test -p <crate>`.
- Comments explain WHY only; keep any `// ponytail:` marker you edit near.
- Commit subjects are imperative sentence case, no attribution trailers of any kind.

---

### Task 1: Tie `ws_namespace` to the admission policy's prefix set (failing test)

**Files:** Modify `crates/workspaces/tests/crd_yaml.rs` (append after the existing `no_two_owner_team_pairs_share_a_namespace` test, which ends around `:200`); reads `deploy/k3s/agent-admission.yaml:81-95`.
**Interfaces:** Consumes `kloudlite_workspaces::crd::{ws_namespace, env_namespace}` and the text of `deploy/k3s/agent-admission.yaml`. Produces test `every_namespace_the_code_makes_is_admitted`.

- [ ] **Step 1: Append the test to `crates/workspaces/tests/crd_yaml.rs`.** It parses the Namespace branch of policy 2 out of the YAML text (no `serde_yaml` — see the file's header comment on RUSTSEC-2024-0320) and asserts every namespace the code can mint starts with a prefix that branch admits.

```rust
/// The bug this exists for: a naming rule lives in Rust and the fence that depends on it lives in
/// CEL, so only a cluster apply connects them — team workspaces (`wt-`) were denied at namespace
/// create, at the host-key Secret and at both RoleBindings for as long as the policy said `ws-`
/// and `env-` only.
#[test]
fn every_namespace_the_code_makes_is_admitted() {
    use kloudlite_workspaces::crd::{env_namespace, ws_namespace};
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/k3s/agent-admission.yaml");
    let policy = std::fs::read_to_string(path).unwrap();
    // Only the Namespace arm of policy 2's expression: the Secret and RoleBinding arms test
    // `metadata.namespace` and are asserted separately below.
    let start = policy.find("object.kind == 'Namespace'").expect("policy 2 has a Namespace branch");
    let end = policy[start..].find("object.kind == 'Secret'").expect("…followed by the Secret branch") + start;
    let prefixes: Vec<String> = policy[start..end]
        .match_indices("startsWith('")
        .map(|(i, m)| {
            let rest = &policy[start + i + m.len()..];
            rest[..rest.find('\'').expect("a closed CEL string literal")].to_string()
        })
        .collect();
    assert!(!prefixes.is_empty(), "no startsWith literals found — did the expression change shape?");

    let mut made = vec![env_namespace("env-abc123")];
    for (owner, team) in [("alice", ""), ("alice", "alice"), ("alice", "acme"), ("a-b", "b-c")] {
        made.push(ws_namespace(owner, team));
    }
    for ns in &made {
        assert!(
            prefixes.iter().any(|p| ns.starts_with(p.as_str())),
            "namespace {ns} is denied by agent-admission.yaml (admits {prefixes:?})"
        );
    }
    // The Secret and RoleBinding arms gate on `metadata.namespace`, so every workspace prefix must
    // appear in each of them too — a team workspace's `ws-ssh-{id}` Secret is denied otherwise.
    let secret_arm = &policy[end..policy.find("object.kind == 'RoleBinding'").expect("a RoleBinding branch")];
    for p in ["ws-", "wt-"] {
        assert!(secret_arm.contains(&format!("startsWith('{p}')")), "Secret branch must admit {p} namespaces");
    }
}
```

- [ ] **Step 2: Run it and confirm it fails for the right reason.** `cargo test -p kloudlite-workspaces --test crd_yaml every_namespace_the_code_makes_is_admitted` must fail with `namespace wt-alice-<tail> is denied by agent-admission.yaml (admits ["ws-", "env-"])`. If it fails with `no startsWith literals found`, the expression was reshaped and the parse anchors need updating before continuing.
- [ ] **Step 3: Commit** with `git add crates/workspaces/tests/crd_yaml.rs && git commit -m "Add a failing test tying ws_namespace to the admission prefix set"`.

---

### Task 2: Widen and tighten both admission policies, the api Secret grant, the peer policy and the image pins

Covers H1, H2, H3, H4 (summary High #1, #2, #3, #7), M1, M2, M3, M4, M6, M7, M10. These apply together on purpose: widening the agent's tenant-namespace policy to `pods` while the `wt-` branches are still missing would deny every team workspace pod as well as its namespace, and adding the gVisor requirement without the pods rule leaves the kube-system escape open. One dry-run, one apply, one commit.

**Files:**
- Modify `deploy/k3s/agent-admission.yaml:26-32` (policy 1 resourceRules), `:36-44` (policy 1 validation), `:54-60` (policy 2 header), `:67-76` (policy 2 resourceRules), `:80-95` (policy 2 validation)
- Modify `deploy/k3s/workspace-admission.yaml:26-31` (the prefix comment), `:49-70` (validations; add two more)
- Modify `deploy/k3s/api-rbac.yaml:63-68` (`kloudlite-api-secrets` rules)
- Modify `deploy/k3s/agent-peer.yaml:9-11` (comment), `:29-36` (ingress)
- Modify `deploy/k3s/agent-daemonset.yaml:163-164` (`WS_GIT_INIT_IMAGE`), `deploy/k3s/zerofs.yaml:104-111` (zerofs image)
- Modify `deploy/k3s/agent-rbac.yaml:53,55` (header table counts)
- Create `deploy/k3s/system-netpol.yaml`
- Modify `deploy/k3s/README.md:15-16` (the file table) and `:51` (the apply line)
- Test: `crates/workspaces/tests/crd_yaml.rs` (Task 1's test turns green), plus `kubectl apply --dry-run=server`

**Interfaces:** Consumes `kloudlite.io/owner`, the `ws-`/`wt-`/`env-` prefixes, the seven `hardened()` capabilities, `WS_RUNTIME_CLASS=gvisor` (already set in the `kloudlite-agent` Secret). Produces ValidatingAdmissionPolicies `kloudlite-agent-spec-is-read-only`, `kloudlite-agent-tenant-namespaces-only`, `kloudlite-workspace-pod-fence`; NetworkPolicies `zerofs-default-deny` and `zerofs-nfs-from-agents` in `kloudlite-system`.

- [ ] **Step 1: H2 — admit `wt-` in all three branches of policy 2, and extend the same policy to the five unpinned kinds (H1).** Replace `deploy/k3s/agent-admission.yaml:67-95` (the `matchConstraints`/`matchConditions`/`validations` of `kloudlite-agent-tenant-namespaces-only`) with:

```yaml
  matchConstraints:
    resourceRules:
      - apiGroups: [""]
        apiVersions: ["v1"]
        operations: ["CREATE", "UPDATE"]
        resources: ["namespaces", "secrets", "pods", "services", "limitranges"]
      - apiGroups: ["apps"]
        apiVersions: ["v1"]
        operations: ["CREATE", "UPDATE"]
        resources: ["statefulsets"]
      - apiGroups: ["networking.k8s.io"]
        apiVersions: ["v1"]
        operations: ["CREATE", "UPDATE"]
        resources: ["networkpolicies"]
      - apiGroups: ["rbac.authorization.k8s.io"]
        apiVersions: ["v1"]
        operations: ["CREATE", "UPDATE"]
        resources: ["rolebindings"]
  matchConditions:
    - name: from-the-agent
      expression: "request.userInfo.username == 'system:serviceaccount:kube-system:kloudlite-agent'"
  validations:
    - expression: >-
        object.kind == 'Namespace'
          ? (object.metadata.name.startsWith('ws-')
             || object.metadata.name.startsWith('wt-')
             || object.metadata.name.startsWith('env-'))
        : object.kind == 'Secret'
          ? ((object.metadata.namespace.startsWith('ws-') || object.metadata.namespace.startsWith('wt-'))
             && object.metadata.name.startsWith('ws-ssh-'))
        : object.kind == 'RoleBinding'
          ? ((object.metadata.namespace.startsWith('ws-')
              || object.metadata.namespace.startsWith('wt-')
              || object.metadata.namespace.startsWith('env-'))
             && object.roleRef.kind == 'ClusterRole'
             && object.roleRef.name in ['kloudlite-api-secrets', 'kloudlite-agent-ws-secrets']
             && has(object.subjects)
             && object.subjects.all(s, s.kind == 'ServiceAccount'
                                       && s.namespace == 'kube-system'
                                       && s.name in ['kloudlite-api', 'kloudlite-agent']))
        : (object.kind in ['Pod', 'StatefulSet', 'Service', 'NetworkPolicy', 'LimitRange'])
          ? (object.metadata.namespace.startsWith('ws-')
             || object.metadata.namespace.startsWith('wt-')
             || object.metadata.namespace.startsWith('env-'))
        : false
      message: "kloudlite-agent may only write ws-/wt-/env- namespaces and namespaced objects inside them, ws-ssh-* Secrets, and the two api/agent secret RoleBindings"
```

- [ ] **Step 2: Correct policy 2's header to say what it now covers.** Replace `deploy/k3s/agent-admission.yaml:54-60` with:

```yaml
# 2. `bind` on a ClusterRole plus `rolebindings: create` is, to RBAC, "grant that role to any
#    subject in any namespace" — including the agent itself in kube-system, which would be
#    `secrets` there. The same shape is true of `pods: create`, which the role holds cluster-wide:
#    a privileged `hostPath: /` pod in kube-system turns root-on-one-node into root-on-the-cluster
#    and mints any user's token, and the pod fence in workspace-admission.yaml cannot refuse it
#    because that binding is scoped to namespaces carrying `kloudlite.io/kind`. So this pins EVERY
#    namespaced kind the agent writes — pods, statefulsets, services, networkpolicies, limitranges,
#    secrets, rolebindings — to the tenant namespaces it makes (`ws-`/`wt-` for workspaces, `env-`
#    for environments; `wt-` is the team pair, see `crd::ws_namespace`), every RoleBinding to the
#    two roles and two subjects the design names, and every Secret to the host key it creates.
#    Any other kind reaching this policy is denied outright: a new write in the code needs a row
#    in agent-rbac.yaml AND a branch here.
#
#    Not covered, deliberately: DELETE. `object` is null on a delete, which needs a second
#    expression shape, and the residual is denial of service on a kube-system pod the kubelet
#    recreates — not a privilege gain like CREATE is.
```

- [ ] **Step 3: M3 — extend policy 1 to snapshots and volumereplicas and to CREATE.** Replace `deploy/k3s/agent-admission.yaml:26-44` (policy 1's `resourceRules` through its validation) with:

```yaml
    resourceRules:
      # Main resources only: `workspaces` does not match `workspaces/status`, which is the
      # controller's half and is meant to change on every reconcile. `snapshots` and
      # `volumereplicas` are here because the agent holds `patch` on both: a Snapshot's `parent` is
      # the history chain `/v1` reads back at `GET /v1/volumes/{name}/history`, so an unchecked
      # patch grafts that chain.
      - apiGroups: ["kloudlite.io"]
        apiVersions: ["*"]
        operations: ["CREATE", "UPDATE"]
        resources: ["workspaces", "environments", "volumes", "ownerbindings", "snapshots", "volumereplicas"]
  matchConditions:
    - name: from-the-agent
      expression: "request.userInfo.username == 'system:serviceaccount:kube-system:kloudlite-agent'"
  validations:
    # A create has no oldObject, so it gets its own arm. The only Volume the agent authors is a
    # PARENT'S CHILD (`ensure_child_volume`, which sets an ownerReference and lets garbage
    # collection do the deleting); a free-standing Volume with an arbitrary region, quota or source
    # is `/v1`'s to write, never the controller's.
    - expression: >-
        request.operation == 'CREATE'
          ? (object.kind != 'Volume'
             || (has(object.metadata.ownerReferences) && size(object.metadata.ownerReferences) > 0))
          : (object.kind == 'Volume'
               ? (object.spec.all(k, k == 'restoreTo'
                                     || (k == 'nodeName' && (oldObject.spec.nodeName == '' || object.spec.nodeName == ''))
                                     || (k in oldObject.spec && object.spec[k] == oldObject.spec[k]))
                  && oldObject.spec.all(k, k == 'restoreTo' || k in object.spec))
               : object.spec == oldObject.spec)
      message: "kloudlite-agent writes status, not spec (exceptions: Volume.spec.restoreTo, Volume.spec.nodeName owned->'' or ''->node, and a created Volume must be a parent's child)"
```

- [ ] **Step 4: H4 and M1 — require the runtimeClass and bound capability adds in the pod fence.** Append these two validations to `deploy/k3s/workspace-admission.yaml` after the hostPath validation (currently ending `:70`):

```yaml
    # gVisor is what makes tenant isolation three independent things instead of two. It is live —
    # `WS_RUNTIME_CLASS=gvisor` in the `kloudlite-agent` Secret, read into `PodContext.runtime_class`
    # — but until this validation existed the kernel boundary rested on that one env var: unset it
    # (a Secret edit, a restored backup, a fresh cluster) and every tenant pod silently drops back to
    # runc, sharing the host kernel, with no alert anywhere. Now the FENCE holds it, and an agent
    # rolled out against a cluster without gVisor fails loudly at pod create rather than quietly.
    - expression: "has(object.spec.runtimeClassName) && object.spec.runtimeClassName == 'gvisor'"
      message: "workspace/environment pods must run under the gvisor RuntimeClass"
    # `k8s::hardened` drops ALL and adds back exactly these seven, and its own comment says the
    # fixed list is "the only thing" stopping SYS_ADMIN/NET_RAW/SYS_PTRACE now that the namespace
    # floor is `privileged`. This is the second thing: a pod builder bug or an operator with kubectl
    # cannot widen the set in a tenant namespace. Keep in step with `crates/workspaces/src/k8s.rs`.
    - expression: >-
        (object.spec.containers
           + (has(object.spec.initContainers) ? object.spec.initContainers : [])
           + (has(object.spec.ephemeralContainers) ? object.spec.ephemeralContainers : []))
          .all(c, !has(c.securityContext)
                  || !has(c.securityContext.capabilities)
                  || !has(c.securityContext.capabilities.add)
                  || c.securityContext.capabilities.add.all(a, a in ['CHOWN', 'DAC_OVERRIDE', 'FOWNER',
                                                                    'SETGID', 'SETUID', 'NET_BIND_SERVICE',
                                                                    'SYS_CHROOT']))
      message: "workspace/environment containers may add only the seven capabilities k8s::hardened grants"
```

- [ ] **Step 5: M2 — replace the per-pool hostPath rule with per-subtree, owner-scoped prefixes.** Replace `deploy/k3s/workspace-admission.yaml:63-70` (the hostPath validation) with:

```yaml
    - expression: >-
        has(object.metadata.labels) && 'kloudlite.io/owner' in object.metadata.labels
          && (has(object.spec.volumes) ? object.spec.volumes : [])
            .all(v, !has(v.hostPath)
                    || (!v.hostPath.path.contains('..')
                        && (v.hostPath.path == '/nix'
                            || v.hostPath.path.startsWith('/nix/')
                            || v.hostPath.path.startsWith('/wspool-prod/vol/')
                            || v.hostPath.path.startsWith('/wspool-prod/attach/')
                            || v.hostPath.path == '/wspool-prod/homes/' + object.metadata.labels['kloudlite.io/owner']
                            || v.hostPath.path.startsWith('/wspool-prod/homes/' + object.metadata.labels['kloudlite.io/owner'] + '/')
                            || v.hostPath.path == '/wspool-prod/homecache/' + object.metadata.labels['kloudlite.io/owner']
                            || v.hostPath.path.startsWith('/wspool-prod/homecache/' + object.metadata.labels['kloudlite.io/owner'] + '/'))))
      message: "workspace/environment hostPath volumes must be /nix, under /nix/, under /wspool-prod/vol|attach/, or this owner's own homes/homecache directory"
```

  Then replace the prefix paragraph at `:26-31` with:

```yaml
# The prefixes are deliberately ASYMMETRIC. `/nix` is allowed EXACTLY, because `k8s::nix_volume`
# mounts `NIX_ROOT` itself and picks the store and the profile out of it by `subPath` — a
# `startsWith('/nix/')` rule alone rejects every workspace pod this cluster builds. `/wspool-prod`
# is NOT allowed exactly, only its four known subtrees: `vol/` (worktrees), `attach/` (the rendered
# resolv.conf), `homes/` and `homecache/`. The last two are additionally scoped to the pod's own
# `kloudlite.io/owner` label, because those two paths are keyed BY OWNER — a builder bug naming
# another owner is a whole other person's dotfiles and shell history. `vol/` and `attach/` are keyed
# by volume and workspace id, not by owner, so CEL has nothing to compare there and the pod builders
# remain the only thing keeping tenants apart on those two; that residual is deliberate and known.
# A pod with no owner label is refused outright — `k8s::meta` stamps it on every object this system
# creates, so a pod without it did not come from a builder.
# `/wspool-prod` must stay in step with `WS_POOL` in `agent-daemonset.yaml`; a mismatch refuses
# every pod rather than failing open.
```

- [ ] **Step 6: H3 — create `deploy/k3s/system-netpol.yaml`,** the default-deny plus one ingress rule for the NFS export, same shape as `agent-peer.yaml`:

```yaml
# `kloudlite-system` holds two things with nothing in common: ZeroFS, which serves EVERY owner's
# home in the region over NFSv3, and the gateway, which accepts unauthenticated connections from
# the internet. NFSv3 with AUTH_SYS and `nolock` (see `mount_homes`) authenticates nothing at all,
# so "who can reach 2049" IS the authorization: anyone who can open that socket reads and writes
# every owner's dotfiles, shell history and SSH material. Until this file existed the only thing
# keeping tenant pods off it was their own egress policy's RFC-1918 exclusion
# (`k8s::allow_internet_egress`) — one NetworkPolicy edit away from the crown jewels, with the
# internet-facing gateway a pod away in the same namespace.
#
# Two objects, because NetworkPolicies are additive and a default-deny alone would also cut the
# gateway: deny all ingress in the namespace, then admit 2049 to ZeroFS from the agent DaemonSet
# only. The gateway's own ingress is a `hostPort` on the node, which NetworkPolicy does not gate,
# so it is unaffected by the deny — see gateway.yaml.
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: zerofs-default-deny
  namespace: kloudlite-system
spec:
  podSelector:
    matchLabels:
      app: zerofs
  policyTypes:
    - Ingress
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: zerofs-nfs-from-agents
  namespace: kloudlite-system
spec:
  podSelector:
    matchLabels:
      app: zerofs
  policyTypes:
    - Ingress
  ingress:
    - from:
        # `mount_homes` runs in the agent pod itself, so the mount's client is the agent's pod IP.
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
          podSelector:
            matchLabels:
              app: kloudlite-agent
      ports:
        - protocol: TCP
          port: 2049
```

  Note for the reviewer of this step: `namespaceSelector` and `podSelector` are ONE peer here (no `-` before `podSelector`). The two-peer form means "every pod in kube-system OR every `kloudlite-agent` pod anywhere" — the same trap the `attach_egress` comment in `k8s.rs:1291` names.

- [ ] **Step 7: M4 — name the two Secrets the api may write.** Replace `deploy/k3s/api-rbac.yaml:63-68` with:

```yaml
rules:
  - apiGroups: [""]
    resources: ["secrets"]
    # Named, not the whole namespace: the same namespace holds the agent's `ws-ssh-{id}` host key,
    # which the agent creates once and NEVER updates because a host key that changed is
    # indistinguishable from a man in the middle (see agent-rbac.yaml). Without `resourceNames` the
    # api could rewrite it, and `kl` adopts whatever key the api hands back — an api compromise
    # would MITM every workspace ssh session silently.
    resourceNames: ["git-token", "user-key"]
    # `patch` is what server-side apply needs; without it the key install fails with a 403 that
    # reads like a missing binding. No `delete`: both Secrets carry an ownerReference and are
    # reclaimed with their namespace.
    verbs: ["get", "create", "update", "patch"]
```

  `resourceNames` cannot restrict `create` (the name is not known at authorization time), so keep `create` and accept that the api may create an arbitrarily named Secret in a workspace namespace; it may not touch an existing `ws-ssh-*` one, which is the finding. Record that in the same comment.

- [ ] **Step 8: M6 — stop the peer policy from blocking metrics.** Replace `deploy/k3s/agent-peer.yaml:9-11` with:

```yaml
# Scope, stated so nobody over-reads it: a NetworkPolicy with `policyTypes: [Ingress]` selecting
# these pods denies ALL other ingress to them, not just what its rules name — so this file must
# list every port the agent serves, and the metrics rule below is not optional decoration: without
# it the `prometheus.io/scrape` on 9464 in the daemonset stops working the moment this is applied.
# NetworkPolicies are additive, so a later policy selecting these pods with a broader `from`
# silently widens this one.
```

  and append a second ingress rule after `:36`:

```yaml
    - from:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: monitoring
      ports:
        - protocol: TCP
          port: 9464
```

- [ ] **Step 9: M7 — pin the two mutable tags by digest.** In `deploy/k3s/agent-daemonset.yaml:163-164`, keep the comment and change the value to `alpine/git:2.45.2@sha256:<digest>`; in `deploy/k3s/zerofs.yaml:111`, change to `ghcr.io/barre/zerofs:2.3.2@sha256:<digest>` and rewrite the `sha-b66813e` sentence at `:108-110` to say the digest is now the pin. Resolve both digests first and paste the real values:

```sh
docker buildx imagetools inspect alpine/git:2.45.2 --format '{{.Manifest.Digest}}'
docker buildx imagetools inspect ghcr.io/barre/zerofs:2.3.2 --format '{{.Manifest.Digest}}'
```

  The `alpine/git` one is the init container that clones with the owner's platform key, so a moved tag runs attacker code next to that credential — that is the WHY to leave in the comment.

- [ ] **Step 10: M10 — correct the two header-table counts.** In `deploy/k3s/agent-rbac.yaml`, line 53 reads `#   */status (kloudlite.io, all five)` — the rule at `:159-163` grants six (`snapshots/status` included); change to `all six`. Line 55 reads `#   */finalizers (kloudlite.io, all five)` — the rule at `:170-174` grants four; change to `all four (Snapshot has none)`.
- [ ] **Step 11: Add the new file to the README.** In `deploy/k3s/README.md`, add a row after the `workspace-admission.yaml` row at `:16`: `| \`system-netpol.yaml\` | Default-deny plus the one 2049-from-agents rule in \`kloudlite-system\`: the NFS home export authenticates nothing, so reachability is the authorization. Apply with \`zerofs.yaml\`. |`, and append ` -f system-netpol.yaml` to the apply line at `:51`.
- [ ] **Step 12: Verify.** Run, in order:
  - `cargo test -p kloudlite-workspaces --test crd_yaml` — Task 1's test must now pass.
  - `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f deploy/k3s/agent-admission.yaml -f deploy/k3s/workspace-admission.yaml -f deploy/k3s/api-rbac.yaml -f deploy/k3s/agent-peer.yaml -f deploy/k3s/system-netpol.yaml -f deploy/k3s/agent-daemonset.yaml -f deploy/k3s/zerofs.yaml -f deploy/k3s/agent-rbac.yaml` — every object `(server dry run)`, no CEL compile error. A CEL mistake surfaces here as `spec.validations[N].expression: Invalid value: …`, which is the whole reason for the dry run.
  - Then, against the live cluster: `KUBECONFIG=.local/k3s.yaml kubectl get pod -A -l kloudlite.io/kind -o jsonpath='{range .items[*]}{.metadata.namespace}{" "}{.spec.runtimeClassName}{"\n"}{end}'` must print `gvisor` on every row BEFORE applying — a running pod without it would be refused on its next update.
- [ ] **Step 13: Commit** with `git add deploy/k3s crates/workspaces/tests/crd_yaml.rs && git commit -m "Pin every namespaced object the agent writes to tenant namespaces"`.

---

### Task 3: Bound ZeroFS and nix-daemon, and document the ZeroFS failover

Covers M8, M9. Separate from Task 2 because both numbers must be measured on the cluster, and a wrong memory limit OOM-kills the pod every home in the region depends on.

**Files:** Modify `deploy/k3s/zerofs.yaml:116-131` (resources and probes), `deploy/k3s/agent-daemonset.yaml:271-286` (nix-daemon container), `deploy/k3s/README.md` (a new failover section).
**Interfaces:** Consumes the measured working-set numbers; produces `resources.limits` on both containers, a `livenessProbe` on ZeroFS, and a README "ZeroFS failover" section.

- [ ] **Step 1: Measure.** `KUBECONFIG=.local/k3s.yaml kubectl top pod -n kloudlite-system -l app=zerofs` and `kubectl top pod -n kube-system -l app=kloudlite-agent --containers`, each sampled a few times across a working hour. Write the observed peaks into the commit body; the limits are 2× the observed peak, rounded up to a round Gi/m.
- [ ] **Step 2: Add a memory limit and a real liveness probe to ZeroFS.** In `deploy/k3s/zerofs.yaml`, replace the `resources` block (`:116-119`, currently requests only) and add a liveness probe beside the readiness probe at `:127-131`:

```yaml
          resources:
            requests:
              cpu: 500m
              memory: 1Gi
            limits:
              # No CPU limit on purpose — a throttled NFS server stalls every home in the region
              # rather than shedding load. The MEMORY limit is the point: this pod is pinned to the
              # control plane, so an unbounded working set pushes the node into pressure and takes
              # the API server with it. <2x the peak measured on YYYY-MM-DD>.
              memory: <measured>Gi
          # Two probes, two questions. Readiness is "is the socket open" — that is exactly what an
          # agent's `mount` needs and no more. Liveness must ask something a wedged-but-listening
          # process cannot answer, so it does a real NULL RPC: a TCP accept alone would keep a
          # deadlocked SlateDB replay in service forever.
          livenessProbe:
            exec:
              command: ["/bin/sh", "-c", "rpcinfo -T tcp 127.0.0.1 100003 3 >/dev/null"]
            initialDelaySeconds: 60
            periodSeconds: 30
            failureThreshold: 3
```

  If `rpcinfo` is not in the image (`kubectl exec -n kloudlite-system deploy/zerofs -- which rpcinfo`), fall back to `nc -z 127.0.0.1 2049` only as a last resort and say in the comment that it is no better than the readiness probe; prefer adding a probe the image can actually answer.

- [ ] **Step 3: Bound nix-daemon.** In `deploy/k3s/agent-daemonset.yaml`, add to the `nix-daemon` container after its `securityContext` (`:280-282`):

```yaml
          # The container that runs builds shares the node with tenant pods, and the agent beside it
          # already carries a request. Unbounded, a profile build starves the workspaces on the node
          # it is building for. <From the peak measured on YYYY-MM-DD.>
          resources:
            requests:
              cpu: 500m
              memory: 1Gi
            limits:
              memory: <measured>Gi
```

- [ ] **Step 4: Document the failover.** Append to `deploy/k3s/README.md` a `### ZeroFS failover (manual, by design)` section: ZeroFS is `replicas: 1` with `strategy: Recreate` because SlateDB has one writer, and it is pinned to the control plane by `nodeSelector`; if that node is lost, every home in the region is unavailable until it returns. Recovery is: bring the control-plane node back, or edit the `nodeSelector`/toleration to another node AND confirm the old pod is gone first (`kubectl -n kloudlite-system get pod -l app=zerofs` empty) — two ZeroFS pods on one SlateDB prefix is the fencing the header forbids. Then restart the agents (`kubectl -n kube-system rollout restart ds/kloudlite-agent`) so their NFS mounts re-resolve; without that they answer EIO from stale handles.
- [ ] **Step 5: Verify.** `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f deploy/k3s/zerofs.yaml -f deploy/k3s/agent-daemonset.yaml` — both accepted, and `kubectl explain` is not needed; the dry run rejects a malformed probe.
- [ ] **Step 6: Commit** with `git add deploy/k3s && git commit -m "Bound ZeroFS and nix-daemon and probe ZeroFS for liveness"`.

---

### Task 4: Make `kl`'s host key an actual pin

Covers M5 (summary deploy Medium on `pin_host_key`).

**Files:** Modify `bins/kl/src/config.rs:93-106` (`pin_host_key`) and its test module (`:108-…`); modify `bins/kl/src/ws.rs:27` only if the call site needs the new flag.
**Interfaces:** Consumes `(id, host_key)` from `api::ssh_session`. Produces `config::pin_host_key(id, host_key) -> Result<(), String>` which now FAILS on a changed key, and `KL_ACCEPT_NEW_HOST_KEY=1` as the deliberate override.

- [ ] **Step 1: Write the failing test.** Add to the `mod tests` in `bins/kl/src/config.rs`:

```rust
    /// The finding: `pin_host_key` filtered out any existing line for the id and appended whatever
    /// the api just returned, so a changed key was adopted silently on every connect. Chained with
    /// the api's Secret grant, that is a silent MITM of every workspace ssh session — ssh's own
    /// known_hosts would have shouted.
    #[test]
    fn a_changed_host_key_is_refused_not_adopted() {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("KL_CONFIG_DIR", d.path());
        super::pin_host_key("ws-1", "ssh-ed25519 AAAAfirst").unwrap();
        // Same key again is a no-op, not an error: every ssh re-pins.
        super::pin_host_key("ws-1", "ssh-ed25519 AAAAfirst").unwrap();
        let err = super::pin_host_key("ws-1", "ssh-ed25519 AAAAsecond").unwrap_err();
        assert!(err.contains("HOST KEY"), "{err}");
        assert!(err.contains("ws-1"), "{err}");
        // And the stored line is untouched — a refused pin must not half-write the file.
        let kh = std::fs::read_to_string(super::known_hosts()).unwrap();
        assert!(kh.contains("AAAAfirst"), "{kh}");
        assert!(!kh.contains("AAAAsecond"), "{kh}");
        // The override exists so a legitimately rebuilt workspace is not a support ticket.
        std::env::set_var("KL_ACCEPT_NEW_HOST_KEY", "1");
        super::pin_host_key("ws-1", "ssh-ed25519 AAAAsecond").unwrap();
        std::env::remove_var("KL_ACCEPT_NEW_HOST_KEY");
        assert!(std::fs::read_to_string(super::known_hosts()).unwrap().contains("AAAAsecond"));
        // An id with no stored line is a first sight, which is what a pin is FOR.
        super::pin_host_key("ws-2", "ssh-ed25519 AAAAother").unwrap();
    }
```

- [ ] **Step 2: Run it and confirm it fails.** `cargo test -p kl a_changed_host_key_is_refused_not_adopted` fails at `let err = … .unwrap_err()` with `called \`Result::unwrap_err()\` on an \`Ok\` value: ()`.
- [ ] **Step 3: Implement.** Replace `pin_host_key` at `bins/kl/src/config.rs:93-106` with:

```rust
/// Pins `<id> <host_key>` in the CLI's own known_hosts. A PIN: once an id has a key, a DIFFERENT
/// key is refused, loudly, the way ssh refuses one — the platform telling us a new key is exactly
/// what a compromised api would also say, and adopting it silently is a man in the middle nobody
/// at either end sees. `KL_ACCEPT_NEW_HOST_KEY=1` is the deliberate escape hatch for a workspace
/// that really was rebuilt.
pub fn pin_host_key(id: &str, host_key: &str) -> Result<(), String> {
    let p = known_hosts();
    make_dir(&dir())?;
    let old = std::fs::read_to_string(&p).unwrap_or_default();
    let stored = old
        .lines()
        .find(|l| l.split_whitespace().next().is_some_and(|h| h == id))
        .map(|l| l[id.len()..].trim().to_string());
    match stored {
        Some(k) if k == host_key => return Ok(()),
        Some(k) if std::env::var("KL_ACCEPT_NEW_HOST_KEY").is_err() => {
            return Err(format!(
                "WARNING: REMOTE HOST KEY HAS CHANGED FOR {id}\n\
                 Someone could be eavesdropping on you right now (man-in-the-middle attack), or the \
                 workspace was rebuilt.\n  stored:   {k}\n  offered:  {host_key}\n\
                 If the workspace really was rebuilt, re-run with KL_ACCEPT_NEW_HOST_KEY=1."
            ));
        }
        _ => {}
    }
    let mut out: String = old
        .lines()
        .filter(|l| !l.split_whitespace().next().is_some_and(|h| h == id))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&format!("{id} {host_key}\n"));
    write_atomic(&p, &out)
}
```

- [ ] **Step 4: Run the test and the gates.** `cargo test -p kl` all green, then `cargo clippy --workspace --all-targets --locked -- -D warnings` clean. The `ws.rs:27` call site needs no change — it already propagates the `Result` with `?`, so a changed key aborts the ssh before the handshake, which is the point. Update the comment at `ws.rs:18-19` to say the key is pinned and a change refused, since it currently claims pinning the old code did not do.
- [ ] **Step 5: Commit** with `git add bins/kl && git commit -m "Refuse a changed workspace host key instead of adopting it"`.

---

### Task 5: Narrow the pod-bridge accept and fold the missing firewall rules into the NSG script

Covers M11 and L4.

**Files:** Modify `deploy/k3s/harden-node.sh:59-62` (the `$POD_CIDR`/`cni0` accepts) and its header (`:1-10`); modify `deploy/k3s/provision-azure.sh:21-34` (the NSG rules); modify `deploy/k3s/README.md:448-458` (the prose those rules replace).
**Interfaces:** Consumes `$POD_CIDR`, `$VNET`, `$ADMIN_CIDR`, `$API_CLIENTS`, `$CF_CIDRS` from `env.sh`. Produces the same `/etc/nftables.conf` shape with per-port pod accepts, and NSG rules for 80-from-Cloudflare and 6443-from-api-tier.

- [ ] **Step 1: Replace the two blanket accepts.** In `deploy/k3s/harden-node.sh`, replace lines 61-62 (`ip saddr $POD_CIDR accept` and `iifname "cni0" accept`) with:

```sh
    # The pod bridge is NOT trusted. A blanket accept here admitted every pod — tenant workspace
    # pods included — to the node's kubelet (10250) and API (6443), leaving the tenant egress
    # NetworkPolicy (`k8s::allow_internet_egress`) as the only lock where this file was supposed to
    # be the second one. What the node actually serves pods is: nothing. The agents' peer listener
    # (8444) and metrics (9464) are pod-to-pod through the CNI, not through the host's input hook,
    # and flannel's VXLAN arrives on flannel.1 below.
    #
    # If a node-served port is ever added, add it here as one line — never widen back to `accept`.
    ip saddr $POD_CIDR ct state established,related accept
    iifname "cni0" ct state established,related accept
```

  (The established/related rule at `:53` already covers replies to node-initiated traffic; these keep pod-network return paths explicit for a reader and admit no new connections.) Then add to the header at `:5-7` a line stating the pod network is untrusted here and that a change to this file is the second lock, not the first.

- [ ] **Step 2: Apply it on one node and prove the cluster still works before touching the second.** On a pool node: `sudo bash deploy/k3s/harden-node.sh` (it validates with `nft -c` before loading — `:80`), then from a workspace pod `kl ws ssh <id>` must still work, `kubectl -n kube-system logs ds/kloudlite-agent --tail=20` must show reconciles continuing, and a `kl ws push` must complete (that is the peer path). Only then run it on the other node.
- [ ] **Step 3: Fold the two prose-only rules into `provision-azure.sh`.** After the existing NSG rule block at `:21-34`, add:

```sh
# Two rules that lived only in README prose until now — two firewall layers that can drift silently
# is the failure this closes. `CF_CIDRS` is the same Cloudflare edge list harden-node.sh admits on
# 80; `API_CLIENTS` is the api tier's egress, which needs 6443.
az network nsg rule create -g "$RG" --nsg-name "$NSG" -n allow-http-cloudflare \
  --priority 210 --access Allow --protocol Tcp --direction Inbound \
  --source-address-prefixes $(tr ',' ' ' < "$CF_IPS_FILE") --destination-port-ranges 80
az network nsg rule create -g "$RG" --nsg-name "$NSG" -n allow-apiserver-api-tier \
  --priority 220 --access Allow --protocol Tcp --direction Inbound \
  --source-address-prefixes ${API_CLIENTS//,/ } --destination-port-ranges 6443
```

  Match the flag style and the `$RG`/`$NSG` variable names the surrounding rules already use — read `:21-34` and copy them exactly rather than the placeholders above.

- [ ] **Step 4: Replace the README prose with a pointer.** At `deploy/k3s/README.md:448-458`, cut the hand-run `az network nsg rule create` instructions and leave one sentence: both rules are in `provision-azure.sh`; the NSG and nftables are two layers of the same list and neither is edited by hand.
- [ ] **Step 5: Verify.** `bash -n deploy/k3s/harden-node.sh && bash -n deploy/k3s/provision-azure.sh` (syntax), and on the node `sudo nft list table inet node | grep -c accept` shows the new per-port shape with no bare `iifname "cni0" accept` line: `sudo nft list table inet node | grep 'cni0'` must print only the established/related form.
- [ ] **Step 6: Commit** with `git add deploy/k3s && git commit -m "Stop trusting the pod bridge and script the two prose-only NSG rules"`.

---

### Task 6: Narrow `allow-dns` to CoreDNS

Covers L1. TDD in Rust — this is a policy the code builds, not a manifest.

**Files:** Modify `crates/workspaces/src/k8s.rs:1175-1194` (`allow-dns`); test in the same file's `mod tests`.
**Interfaces:** Produces the `allow-dns` NetworkPolicy JSON with a `podSelector` beside its `namespaceSelector` in ONE peer.

- [ ] **Step 1: Write the failing test** in `crates/workspaces/src/k8s.rs`'s test module:

```rust
    /// `allow-dns` reached every pod in kube-system on 53 — the agent's own DaemonSet included —
    /// where only CoreDNS was ever meant. One peer, both selectors: the two-peer form would mean
    /// "all of kube-system OR every k8s-app=kube-dns pod anywhere", which is wider than what it
    /// replaces (see `attach_egress`'s comment on the same trap).
    #[test]
    fn allow_dns_reaches_coredns_only() {
        let p = policies(&ctx(), "ws-alice", "alice", &owner_ref())
            .into_iter()
            .find(|p| p.metadata.name.as_deref() == Some("allow-dns"))
            .expect("allow-dns");
        let to = &p.spec.as_ref().unwrap().egress.as_ref().unwrap()[0].to.as_ref().unwrap();
        assert_eq!(to.len(), 1, "one peer, or the selectors are an OR");
        assert_eq!(
            to[0].pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap()["k8s-app"],
            "kube-dns"
        );
    }
```

  Adapt `policies(...)`/`ctx()`/`owner_ref()` to the helpers the module's existing tests already use — read the tests around `k8s.rs:1885-1941` and reuse them rather than inventing new fixtures.

- [ ] **Step 2: Run and confirm it fails.** `cargo test -p kloudlite-workspaces allow_dns_reaches_coredns_only` fails at the `pod_selector` unwrap with `called \`Option::unwrap()\` on a \`None\` value`.
- [ ] **Step 3: Implement.** In the `allow-dns` `json!` at `k8s.rs:1183-1192`, replace the `"to"` entry with:

```rust
                    "to": [{
                        "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "kube-system" } },
                        "podSelector": { "matchLabels": { "k8s-app": "kube-dns" } },
                    }],
```

  and extend the comment above it: the namespace label alone admitted every kube-system pod on 53, the agent's peer listener among them; `k8s-app: kube-dns` is CoreDNS's own label in k3s, and the two selectors must stay in ONE peer.

- [ ] **Step 4: Run the gates.** `cargo test -p kloudlite-workspaces` and `cargo clippy --workspace --all-targets --locked -- -D warnings`. Then, on the cluster, from any workspace pod: `getent hosts kubernetes.default` still resolves after the agent rolls.
- [ ] **Step 5: Commit** with `git add crates/workspaces && git commit -m "Narrow the tenant DNS egress policy to CoreDNS"`.

---

### Task 7: Authenticate the control-plane backup

Covers L2.

**Files:** Modify `deploy/k3s/backup-controlplane.sh:66` and the restore instructions in `deploy/k3s/README.md` (the "Control-plane backup" section).
**Interfaces:** Consumes `$KEY_FILE`; produces `k3s-backup.tgz.enc` plus a detached `k3s-backup.tgz.enc.hmac` uploaded beside it.

- [ ] **Step 1: Add the HMAC.** After the `openssl enc` line at `:66`, add:

```sh
# AES-CBC is unauthenticated: a truncated upload or a tampered blob decrypts to garbage rather than
# failing, and a restore drill would find out at the worst moment. A detached HMAC over the
# CIPHERTEXT, keyed by the same file, is the smallest thing that makes a bad blob fail loudly.
# ponytail: encrypt-then-MAC by hand; `age` does both in one tool if this grows a second consumer.
openssl dgst -sha256 -mac HMAC -macopt "hexkey:$(xxd -p -c 256 "$KEY_FILE")" \
  -out "$WORK/k3s-backup.tgz.enc.hmac" "$WORK/k3s-backup.tgz.enc"
```

- [ ] **Step 2: Upload it.** Find the `put` call for `k3s-backup.tgz.enc` (below `:69`) and add the same call for `k3s-backup.tgz.enc.hmac`, so the two always travel together.
- [ ] **Step 3: Document the verify-before-decrypt step** in `deploy/k3s/README.md`'s restore instructions: download both, recompute the HMAC with the same command, `diff` it against the downloaded file, and only then `openssl enc -d`. State that a mismatch means do not restore.
- [ ] **Step 4: Verify.** `bash -n deploy/k3s/backup-controlplane.sh`, then a local round trip: `printf secret > /tmp/k && printf data > /tmp/f && openssl dgst -sha256 -mac HMAC -macopt "hexkey:$(xxd -p -c 256 /tmp/k)" -out /tmp/f.hmac /tmp/f && cat /tmp/f.hmac` prints a digest, and flipping a byte of `/tmp/f` changes it.
- [ ] **Step 5: Commit** with `git add deploy/k3s && git commit -m "Authenticate the control-plane backup with a detached HMAC"`.

---

### Task 8: Document rotating the api tier's k3s kubeconfig

Covers L6.

**Files:** Modify `deploy/k3s/README.md` (new subsection); read `deploy/kloudlite.yaml:452-456,483-488` for the Secret name and mount path first.
**Interfaces:** Consumes the `kloudlite-k3s-kubeconfig` Secret; produces a README "Rotating the api tier's kubeconfig" subsection.

- [ ] **Step 1: Read `deploy/kloudlite.yaml:445-495`** and note the exact Secret name, the ServiceAccount it is minted from, and the `ponytail:` note saying the design replaces it — the rotation doc must not contradict that note.
- [ ] **Step 2: Write the subsection.** Steps, as commands: mint a fresh bound token for the api's ServiceAccount (`kubectl -n kube-system create token kloudlite-api --duration=8760h`), build the kubeconfig around it with the cluster's CA, `kubectl create secret generic kloudlite-k3s-kubeconfig --from-file=... --dry-run=client -o yaml | kubectl apply -f -` in the AKS cluster, `kubectl rollout restart deploy/kloudlite-api` there, verify with a `/v1/regions` read, then delete the old token. State the cadence (yearly, or immediately on any suspicion) and that this is a stopgap the `ponytail:` note names.
- [ ] **Step 3: Verify.** `grep -n "kloudlite-k3s-kubeconfig" deploy/k3s/README.md deploy/kloudlite.yaml` shows the README section referencing the same Secret name the manifest uses — a doc naming a Secret that does not exist is worse than no doc.
- [ ] **Step 4: Commit** with `git add deploy/k3s/README.md && git commit -m "Document rotating the api tier's k3s kubeconfig"`.

---

## Self-review

| Finding | Task |
|---|---|
| H1 (agent SA can create a privileged kube-system pod) — summary High #2 | Task 2, steps 1-2 |
| H2 (`wt-` namespaces denied) — summary High #1 | Task 1 + Task 2, step 1 |
| H3 (unauthenticated NFS export, no NetworkPolicy) — summary High #3 | Task 2, step 6 |
| H4 (nothing requires the runtimeClass) — summary High #7, reduced per the summary's correction: gVisor is already live via `WS_RUNTIME_CLASS=gvisor` in the `kloudlite-agent` Secret, so only the admission validation is planned; no rollout | Task 2, step 4 |
| M1 (pod fence does not refuse capability adds) | Task 2, step 4 |
| M2 (hostPath allow-list per-pool, not per-tenant) | Task 2, step 5 |
| M3 (spec-read-only policy misses CREATE, snapshots, volumereplicas) | Task 2, step 3 |
| M4 (`kloudlite-api-secrets` full CRUD on every Secret) | Task 2, step 7 |
| M5 (`kl` re-writes the host key; not a pin) | Task 4 |
| M6 (`agent-peer` blocks the metrics port) | Task 2, step 8 |
| M7 (two images pinned by mutable tag) | Task 2, step 9 |
| M8 (ZeroFS SPOF, no memory limit, no liveness probe) | Task 3 |
| M9 (`nix-daemon` unbounded) | Task 3, step 3 |
| M10 (RBAC header table miscounts) | Task 2, step 10 |
| M11 (`harden-node.sh` admits every port from the pod bridge) | Task 5, steps 1-2 |
| L1 (`allow-dns` reaches every kube-system pod) | Task 6 |
| L2 (unauthenticated AES-CBC backup) | Task 7 |
| L3 (gateway `jti` replay across replicas) | **deferred:** per-replica by design and already `ponytail:`-marked at `bins/gateway/src/tunnel.rs:40-41`; global single-use needs Redis in the gateway's request path, and the token's 60 s TTL plus the per-IP edge limit is the stated mitigation — revisit only when the gateway grows a Redis dependency for another reason. |
| L4 (NSG vs nftables rules kept in prose) | Task 5, steps 3-4 |
| L5 (`env.example.sh` duplicates the git-ignored `env.sh`) | **deferred:** the reviewer flagged it as "fine as a template", not a defect — a checked-in example beside a git-ignored real file is the intended shape, and deleting it costs a new operator the template. |
| L6 (long-lived k3s kubeconfig for the api tier, no rotation path) | Task 8 |
| Summary "Tests worth adding" #7 (every `ws_namespace` output satisfies the policy's prefix set) | Task 1 |
