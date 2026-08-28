# The k3s workload cluster

Workspaces and environments run here. The git/registry server tier does not — it stays on its own
cluster and this one reaches it over HTTP.

Files, in the order a cluster is built:

| File | What it does |
| --- | --- |
| `env.example.sh` | Copy to `env.sh` (git-ignored) and edit. Sizes, names, operator CIDR. |
| `provision-azure.sh` | VNet, NSG, control plane, workers, build VM. Idempotent — re-run after a partial failure. |
| `format-pool.sh` | Run on each worker with the data disk as argument. btrfs at `/wspool-prod`. |
| `crds.yaml` | **Generated** — do not hand-edit. `CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml`. |
| `storageclass.yaml` | The class every workspace volume binds through. |
| `agent-rbac.yaml` | ServiceAccount + ClusterRole for the node controller. |
| `agent-daemonset.yaml` | The node controller itself, one pod per pooled node. |
| `harden-node.sh` | Node firewall (drop-by-default on the public NIC), unattended upgrades, keys-only sshd. Idempotent; run on every node after provisioning and after changing the operator CIDR, and again with `CF_CIDRS` set once the gateway is live. Streamed over `ssh … sudo bash -s < harden-node.sh`, so `CF_CIDRS` must be passed as an env var on the remote command, not read from a local file — see the Gateway section below. |
| `cloudflare-ips-v4.txt` | Cloudflare's published v4 edge ranges, one CIDR per line — build `CF_CIDRS` from it locally (`paste -sd, cloudflare-ips-v4.txt`) before running `harden-node.sh`. Refresh from https://www.cloudflare.com/ips-v4 when Cloudflare announces a change; a stale list fails safe (the new edge is just refused, never wrongly trusted). |
| `gateway.yaml` | The workspace SSH gateway: one pod per pool node on the node's own `hostPort: 80`, behind the Cloudflare proxy (TLS ends at the edge). |
| `rotate-agent-token.sh` | Mint a new region agent token at the api and install it in the DaemonSet in one step. |
| `nix-conf.yaml` | ConfigMap: the host Nix daemon's substituters, keys and GC headroom. |
| `backup-controlplane.sh` | Hourly SQLite backup to Azure Blob. Restore procedure is in the script's trailing comment. |

The controller's image is built from the repo-root `Dockerfile` (`agent` target) by
`.github/workflows/image.yml` — both images come out of one compile.


## Iterating

CI is the wrong loop for iteration: it starts from a cold Actions cache and builds with the
production profile. `dev-push.sh` builds on the build VM instead, with the `dev-image` profile (no
LTO, 16 codegen units) against a warm cargo target, and rolls the DaemonSet:

```sh
BUILD_HOST=azureuser@<build-vm> ./dev-push.sh
```

Measured on this repo: **1m57s incremental**, against ~8 minutes through CI. Images are tagged
`dev-{short-sha}` (plus `-dirty` for uncommitted work) so one can never be mistaken for a CI
artifact. Deploy manifests still pin CI's SHA tags.

## Applying

```sh
kubectl apply -f crds.yaml -f storageclass.yaml -f agent-rbac.yaml -f nix-conf.yaml -f agent-daemonset.yaml -f gateway.yaml
```

Nodes need labels before the DaemonSet will schedule and before placement will pick them:

```sh
kubectl label node <node> rustic-git.io/pool=true          # has a btrfs pool: run the controller here
kubectl label node <node> rustic-git.io/session=true       # may host workspaces
kubectl label node <node> rustic-git.io/env=true           # may host environments
```

One key per role, not `role=session`, because a label key holds one value and a small cluster needs
one node to be both.

## Release 1: controller ownership

The 2026-08-27 change: the API writes ONE unplaced object, the agents claim it through
`status.nodeName`, and the Volume becomes a child of its Workspace. The CRDs and the agent move
together — the old agent's watch 4xx's between them — so steps 2–4 are one operation, not a
change with a soak in the middle. The k3s side uses `KUBECONFIG=.local/k3s.yaml`; the API tier
lives on AKS in the `rustic-git` namespace, on the default context.

```sh
# 1. The stuck pre-migration workspace. It predates status.nodeName and no controller can converge
#    it; deleting it before the roll keeps it out of the migration's logs.
KUBECONFIG=.local/k3s.yaml kubectl delete workspace ws-16980a570dd6eecd

# 2. CRDs first, or the agent's placement watch (a field selector on .status.nodeName) is refused
#    and the agent comes up converging nothing while reporting healthy. Already applied on dev.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/crds.yaml

# 3. RBAC. Already applied on dev; harmless to re-apply.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-rbac.yaml -f deploy/k3s/api-rbac.yaml

# 4. The agent, immediately after the CRDs — same operation. Repin the image tag to the SHA CI
#    built first (image.yml), then apply and wait for the DaemonSet to finish.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/nix-conf.yaml -f deploy/k3s/agent-daemonset.yaml
KUBECONFIG=.local/k3s.yaml kubectl rollout status ds/rustic-git-agent -n kube-system

# 5. Watch the startup migration adopt the existing objects. Every line it writes is prefixed
#    `migration:`; every workspace must end with a node in STATUS, not only in spec.
KUBECONFIG=.local/k3s.yaml kubectl logs -n kube-system -l app=rustic-git-agent --tail=200 \
  | grep migration:
KUBECONFIG=.local/k3s.yaml kubectl get workspaces \
  -o custom-columns=NAME:.metadata.name,SPEC:.spec.nodeName,STATUS:.status.nodeName,VOL:.status.volumeRef

# 6. Only then the API tier, on AKS (deploy/rustic-git.yaml, pinned to CI's SHA).
kubectl apply -f deploy/rustic-git.yaml
kubectl rollout status deploy/rustic-git-api -n rustic-git
```

Then verify by hand what `tests/ws_e2e.sh`'s seeded phase proves in CI: use "Open in a workspace"
on a repository, and check the new workspace reaches Ready with the repository cloned into
`/workspace` — that first-workspace clone is the bug this release exists to fix.

Release 1 is reversible: the CRD still carries `spec.nodeName`/`spec.volumeRef`, and an old agent
ignores the new status fields. Release 2 drops those fields and cannot be rolled back, so it waits
until every node has run release 1.

## Gateway

The workspace SSH gateway runs on the pool nodes themselves (`session-0`, `env-0`) behind
Cloudflare — no LoadBalancer, no tunnel connector. Operator steps, once per region:

1. DNS (Cloudflare dashboard): **A** records for `ws-<region>.khost.dev` → each pool node's
   public IP, both **proxied**.
2. SSL/TLS mode **Full (strict)** for the zone.
3. SSL/TLS → Origin Server → Create Certificate (15 years) for `ws-*.khost.dev`, then
   `kubectl -n kube-system create secret tls gateway-tls --cert=<cert> --key=<key>`.
4. Copy the `rustic-git-jwt` Secret from AKS into this cluster's `kube-system` (the gateway
   verifies session tokens locally, with the same secret the api mints them with).
5. `harden-node.sh` on each pool node so the node's 80 admits only Cloudflare's edge. The script
   is streamed over ssh (`sudo bash -s <`), so it has no file of its own on the remote box to read
   a CIDR list from — build `CF_CIDRS` locally and pass it as an env var on the remote command:

   ```sh
   CF_CIDRS="$(paste -sd, deploy/k3s/cloudflare-ips-v4.txt)"
   ssh azureuser@<node> "sudo CF_CIDRS='$CF_CIDRS' ADMIN_CIDR='$ADMIN_CIDR' bash -s" \
     < deploy/k3s/harden-node.sh
   ```

## Two things that bite

**The agent Secret is not in this directory.** `rustic-git-agent` in `kube-system` carries the
region's agent token and the Azure credentials, and it is created by hand because it holds secrets.
Without it the controller runs but every push fails at the registry. Keys: `WS_REGISTRY_URL`,
`WS_REGION`, `WS_AGENT_TOKEN`, `AZURE_ACCOUNT`, `AZURE_KEY`, `AZURE_CONTAINER`.

**Restoring a snapshot pushed in another region needs that region's credentials too.** A
`CommitRecord` names the region its blobs live in, and a `restoreOf` source carries that region
down to the agent. The agent's own `AZURE_*` triple points at ITS region's container only, so a
snapshot from elsewhere is unreadable with it — the restore fails, permanently and by name
(`Ready=False/RegionUnreachable`), rather than sitting in `phase: working` forever, which is what
it used to do.

Add one extra triple per region this node may restore FROM, in the same hand-made Secret, keyed by
the region id uppercased with `-` replaced by `_`:

```
AZURE_REGION_<ID>_ACCOUNT
AZURE_REGION_<ID>_KEY
AZURE_REGION_<ID>_CONTAINER
```

The k3s region's agent needs the `centralindia-vm` triple — `AZURE_REGION_CENTRALINDIA_VM_ACCOUNT`
/ `_KEY` / `_CONTAINER`, pointing at that region's storage account and its `wslayers` container
(the k3s region's own is `wslayers-k3s`) — so environment baselines pushed from the VM region can
be restored here. The values live in the region's `Region` record and in the Azure portal; they
are deliberately not in this repository. A region with no triple is not a failure until someone
restores from it.

**Do not run `tests/ws_e2e.sh` on a node the DaemonSet is running on.** The script starts its own
agent against its own loopback pool; two controllers reconciling one object materialize it into two
different pools. The script refuses to start if it sees the DaemonSet on its node — take the label
off first (`kubectl label node <node> rustic-git.io/pool-`) and put it back after.
