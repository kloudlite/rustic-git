# Attaching a workspace to an environment — design

Date: 2026-08-30. Status: draft for review.

## Goal

A workspace can be attached to one environment. While attached, code running in the workspace
reaches that environment's services by their bare names — `mongodb://db:27017`, `redis:6379` —
exactly as a sibling service inside the environment already does.

Attaching and detaching take effect on a **running** workspace, with no restart.

## What we have that this builds on

- An environment's services are already published as ClusterIP `Service`s in the environment's own
  namespace (`k8s::service_clusterip`), which is what makes `db` resolve between siblings.
- `WORKSPACE_LABEL` already exists on every workspace pod. Its doc says why: workspaces of one
  owner **share a namespace**, so anything granting access to one workspace must select the POD,
  never the namespace.
- `ensure_storage(ns, pv, claim, host_path, access_mode, capacity_gb, owner, pod_ctx, ctx)` already
  builds a `local` PV + PVC pair; the home and Nix mounts are both built with it.
- Workspaces and environments both carry `spec.region`, and a region is a separate cluster.

## Verified facts this design rests on

Each was checked against the live cluster, not assumed. They are the reason the design looks the
way it does, so they are recorded here with what was observed.

1. **A pod's `dnsConfig` cannot be changed after creation.** Patching it is refused:
   `pod updates may not change fields other than spec.containers[*].image, …`. So the search
   domain cannot be edited on a running pod.
2. **`/etc/resolv.conf` inside a container is a writable bind mount.** A truncating write
   (`> file`) succeeds; `sed -i` fails with `Device or resource busy` because it renames.
3. **An edit to it survives a container restart** — restartCount went 0 → 1 with the edit intact.
   Kubelet writes that file once per pod *sandbox*, not per container.
4. **A `volumeMount` at `/etc/resolv.conf` overrides the runtime's own file.** A pod mounting one
   with `subPath` saw the mounted content, not kubelet's.
5. **A host-side in-place write propagates live into that mount; a rename does not.** Writing in
   place changed what the running pod read; replacing the file via `mv` left the pod on the stale
   inode. This inverts the usual "write atomically via rename" instinct.
6. **`hostPath` is refused in a workspace namespace.** It carries
   `pod-security.kubernetes.io/enforce: baseline`, and the API server answers
   `violates PodSecurity "baseline:latest": hostPath volumes`. A `local` PV is the sanctioned
   equivalent, which is why the Nix store already reaches workspaces that way.
7. **The agent is not `hostNetwork`**, so its own `/etc/resolv.conf` is kubelet-generated and
   carries the cluster nameserver, `options ndots:5` and the node's DNS suffix.

## Design

### The attachment

`WorkspaceSpec.attached_environment: Option<String>` — an environment id.
`#[serde(default, skip_serializing_if = "Option::is_none")]`, so existing objects still parse.

One environment, not a list. Bare-name resolution has to be unambiguous: if two attached
environments both exposed `db`, search-domain order would silently decide the winner. A second
attachment can be added later without changing this one.

Spec is desired state and only `/v1` writes it — the agent's admission policy forbids it writing
spec, and this field is not one of the two exceptions.

### Reaching the services by name

Each workspace namespace gets one `local` PV + PVC pair, `attach-{ns}` / `attach`, over the host
path `{pool}/attach`, built by `ensure_storage` like the home pair. Per namespace, not per
workspace, following `home_pv_name`: a local PV binds to exactly one claim, but one claim serves
every pod in the namespace. Each workspace pod mounts it **read-only** at `/etc/resolv.conf` with
`subPath: {ws-id}/resolv.conf`.

The agent owns that file. It is templated from the agent's own `/etc/resolv.conf`, replacing only
the leading search entry:

```
search {env-ns}.svc.cluster.local {ws-ns}.svc.cluster.local svc.cluster.local cluster.local <node suffix>
nameserver <inherited>
options ndots:5
```

Unattached, the environment entry is absent and the file is what kubelet would have written.
Inheriting the rest from the agent's own file means the nameserver, `ndots` and the node suffix are
never synthesised and cannot drift from what the cluster actually uses.

Two rules the implementation must hold, both from the verified facts above, both counter-intuitive
enough to carry a comment saying why:

- **Write in place. Never rename.** A rename leaves every running pod reading the old inode, so
  attachment silently stops working. A future "make this write atomic" refactor is exactly the
  change that would break it.
- **Write the file before creating the pod.** A `subPath` whose target does not exist is created as
  a **directory**, which makes `/etc/resolv.conf` unreadable and breaks all name resolution in the
  workspace. The home volume already imposes an ordering like this on pod creation.

Attach and detach are therefore a file write: live, no pod restart, no `pods/exec` grant, and no
dependency on anything existing inside the image.

### Reaching them over the network

Both namespaces are default-deny in both directions, and `allow_internet_egress` deliberately
excludes RFC 1918, so cross-namespace traffic is blocked on egress *and* ingress today. An
attachment adds exactly two `NetworkPolicy` objects, both named `attach-{ws-id}`:

- in the **workspace** namespace: egress from pods labelled `WORKSPACE_LABEL={ws-id}` to
  `namespaceSelector: kubernetes.io/metadata.name = {env-ns}`;
- in the **environment** namespace: ingress from that namespace selector **and** that pod label.

The pod selector is not optional: workspaces share a namespace, so a namespace-wide grant would
open every workspace the owner has.

`ownerReferences` cannot cross namespaces, so the environment-side policy cannot be owned by the
Workspace. It carries an ownerReference to the **Environment** instead, so deleting the environment
collects it; the workspace reconciler deletes it by name on detach, and the workspace's existing
finalizer deletes it when the workspace goes.

NetworkPolicies are ordinary namespaced objects, so these also take effect without a restart.

### Who may attach

`POST /v1/workspaces/{id}/attach` with `{"environment": "env-…"}`, and
`POST /v1/workspaces/{id}/detach`, alongside the existing `start`/`stop` routes on `bins/api`.

Attach is refused unless: the environment exists; `spec.region` matches the workspace's (a
different region is a different cluster — no route, no DNS); and the caller passes `may_act_on` for
both objects. Environments are team-owned and workspaces user-owned, so this is a real membership
check. Authorization reads `spec.owner`, never a label.

### Lifecycle

| Event | Behaviour |
|---|---|
| Environment stopped | Attachment stays. Names stop resolving until it starts again — the same way a stopped environment already behaves for everything else. |
| Environment started | Names resolve again. Nothing to reconcile: the search domain names the namespace, not a pod. |
| Environment deleted | `/v1` clears `attachedEnvironment` on every workspace attached to it, as part of the delete — only the API may write spec. |
| Workspace stopped | Pod goes; the file and the policies stay. Starting it again comes back attached. |
| Workspace deleted | Finalizer removes `{pool}/attach/{ws-id}` and the environment-side policy. |
| Environment deleted while `/v1` is down mid-delete | The reconciler treats a missing or wrong-region environment as unattached: no search domain, no policies. A dangling grant is never left behind. |

### Status

An `Attached` condition on the workspace: `True` with the environment id, or `False` with
`EnvironmentNotFound`, `EnvironmentStopped` or `RegionMismatch`. Status is agent-written, which is
allowed, and `observedGeneration` already distinguishes "not yet reconciled" from "refused".

## Failure modes

| Failure | Behaviour |
|---|---|
| Attach names an environment in another region | 409 from `/v1`, before anything is written. The reconciler repeats the check and reports `RegionMismatch` if a spec ever arrives with one. |
| The `attach` PVC is not yet bound when the pod is created | Pod stays `Pending` on the claim, as it already does for the home claim. |
| `{ws-id}/resolv.conf` missing at pod creation | Prevented by ordering: the reconciler writes it first. If it is ever absent the mount produces a directory, so the reconciler treats a directory at that path as corrupt, removes it and rewrites the file. |
| The environment exposes no ports | Nothing to resolve. Attachment still succeeds; the services simply have no ClusterIP `Service`. `Service.ports` already defaults to empty for documents written before ports existed. |
| Two services in different environments share a name | Cannot arise: one attachment at a time. |
| A workspace on a node whose pool has no `{pool}/attach` | The reconciler creates the directory; it is agent-owned and outside any user volume, so it is never in a snapshot. |

## Not in scope

Several environments at once; attaching an environment from another region or cluster; mirroring
the environment's services as `ExternalName` objects into the workspace namespace; exposing
environment services to anything other than an attached workspace; a UI for choosing individual
services rather than the whole environment.

## Tests

- `crates/workspaces` units: the CRD field round-trips; `attach_pv_name`/claim naming; the two
  NetworkPolicy builders select the pod by label and the namespace by name; the resolv.conf
  renderer produces the attached and unattached forms from a template.
- `bins/agent/tests/reconcile.rs`: attaching writes both policies and the file; detaching removes
  the environment-side policy and rewrites the file; a missing environment reconciles as
  unattached; a wrong-region spec reports `RegionMismatch`; the pod carries the `subPath` mount;
  deleting the workspace cleans up.
- `crates/workspaces/tests/api_user.rs`: attach refuses a foreign environment (403), a
  cross-region one (409) and an unknown one (404); detach is idempotent.
- `crd_yaml.rs` regenerates `deploy/k3s/crds.yaml`.
- `tests/ws_e2e.sh`: attach a workspace to a running environment and resolve a service by name from
  inside the workspace, then detach and assert it stops resolving — the one place the whole path
  (file, mount, policies, DNS) is exercised together.
