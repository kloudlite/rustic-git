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
| `backup-controlplane.sh` | Hourly SQLite backup to Azure Blob. Restore procedure is in the script's trailing comment. |
| `Dockerfile.agent` | The controller's image. Built by CI (`.github/workflows/image.yml`, `agent` job). |

## Applying

```sh
kubectl apply -f crds.yaml -f storageclass.yaml -f agent-rbac.yaml -f agent-daemonset.yaml
```

Nodes need labels before the DaemonSet will schedule and before placement will pick them:

```sh
kubectl label node <node> rustic-git.io/pool=true          # has a btrfs pool: run the controller here
kubectl label node <node> rustic-git.io/session=true       # may host workspaces
kubectl label node <node> rustic-git.io/env=true           # may host environments
```

One key per role, not `role=session`, because a label key holds one value and a small cluster needs
one node to be both.

## Two things that bite

**The agent Secret is not in this directory.** `rustic-git-agent` in `kube-system` carries the
region's agent token and the Azure credentials, and it is created by hand because it holds secrets.
Without it the controller runs but every push fails at the registry. Keys: `WS_REGISTRY_URL`,
`WS_REGION`, `WS_AGENT_TOKEN`, `AZURE_ACCOUNT`, `AZURE_KEY`, `AZURE_CONTAINER`.

**Do not run `tests/ws_e2e.sh` on a node the DaemonSet is running on.** The script starts its own
agent against its own loopback pool; two controllers reconciling one object materialize it into two
different pools. The script refuses to start if it sees the DaemonSet on its node — take the label
off first (`kubectl label node <node> rustic-git.io/pool-`) and put it back after.
