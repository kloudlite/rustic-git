# Team workspace namespaces and binding names change shape

Ships with the "Workspace names and admission" PR (audit S-1 / S-9). Personal namespaces
(`ws-{owner}`) do not change. Two derived names do:

| object | before | after |
|---|---|---|
| team workspace namespace, `crd::ws_namespace(owner, team)` | `ws-{team}-{owner}` | `wt-{owner}-{hex12}`, hex12 = sha256(`{team}/{owner}`)[..12] |
| `OwnerBinding` name, `crd::binding_name(region, owner)` | `{region}-{owner}` | `{region}-{owner}-{hex12}`, hex12 = sha256(`{region}/{owner}`)[..12] |

Why: handles, team slugs and region ids all allow `-`, so the old joins had two readings — team
`acme` + owner `bob` was the same namespace as personal handle `acme-bob`, and every pod in it
mounted the same fixed-name `user-key` Secret (the owner's private git key).

## What happens on deploy, without operator action

- Every node's OwnerBinding controller derives names at reconcile time, so the next reconcile of
  each binding creates the new `wt-*` namespaces (LimitRange, NetworkPolicies, RoleBinding
  included) for every team the owner has a placed workspace in. Nothing needs to be re-created by
  hand.
- The claim path creates bindings under the new name on the next claim. Old-named bindings keep
  reconciling harmlessly (they carry the same `spec`), so nothing breaks while both exist.
- The API's key install (`refresh_user_keys`, `install_user_key`) recomputes names, so the
  `user-key` Secret lands in the new namespaces on the next key change or workspace list.

## What the operator does

Derived from reading `apply_workspace` / `apply_binding`; not yet exercised on a cluster, so do
it on one team namespace first and watch the workspace come back before doing the rest.

1. Roll the agent DaemonSet and the api Deployment together — an old api writing keys into
   `ws-{team}-{owner}` while a new agent runs pods in `wt-*` is a workspace with no git key.
2. A team workspace that was RUNNING is now half-moved: the reconciler applies its pod and PVC in
   `wt-*`, but the PVC stays `Pending` because the local PV (`live-{id}`, reclaim `Retain`) is
   still bound to the PVC in the old namespace — and the old pod keeps running there, since
   nothing deletes a namespace the controller no longer derives. The data is the btrfs subvolume
   under the PV, untouched by any of this.
3. List the old team namespaces — every `ws-*` workspace namespace whose name is not
   `ws-{owner}` for the owner label it carries:

   ```sh
   kubectl get ns -l kloudlite.io/kind=workspace -o json \
     | jq -r '.items[] | select(.metadata.name != "ws-" + .metadata.labels["kloudlite.io/owner"]) | .metadata.name'
   ```

4. For each: note the workspace ids of its PVCs (`live-{id}`), delete the namespace (pods and
   old PVCs go; PVs become `Released`), then clear each PV's stale `claimRef` so the `wt-*`
   PVC binds and the reconciler brings the pod up:

   ```sh
   kubectl -n <old-ns> get pvc -o name                 # live-ws-...
   kubectl delete ns <old-ns>
   kubectl patch pv live-<id> --type=json -p '[{"op":"remove","path":"/spec/claimRef"}]'
   ```

5. Delete the old-named OwnerBindings — any whose name has no 12-hex tail. They are harmless
   duplicates until then:

   ```sh
   kubectl get ownerbindings -o name | grep -Ev -- '-[0-9a-f]{12}$'
   ```

Personal namespaces are untouched; do not delete anything named `ws-{owner}`.
