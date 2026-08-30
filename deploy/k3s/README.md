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
| `agent-rbac.yaml` | ServiceAccount + ClusterRole for the node controller. The header table is the role: one row per call the agent makes. |
| `agent-admission.yaml` | The ValidatingAdmissionPolicy that makes the role true — refuses the agent any spec write but `Volume.spec.restoreTo`, and pins its Secrets/RoleBindings/Namespaces to `ws-*`/`env-*`. Apply with `agent-rbac.yaml`, always. |
| `agent-daemonset.yaml` | The node controller itself, one pod per pooled node. |
| `harden-node.sh` | Node firewall (drop-by-default on the public NIC), unattended upgrades, keys-only sshd. Idempotent; run on every node after provisioning and after changing the operator CIDR, and again with `CF_CIDRS` set once the gateway is live. Streamed over `ssh … sudo bash -s < harden-node.sh`, so `CF_CIDRS` must be passed as an env var on the remote command, not read from a local file — see the Gateway section below. |
| `cloudflare-ips-v4.txt` | Cloudflare's published v4 edge ranges, one CIDR per line — the one source. Build `CF_CIDRS` from it locally (`paste -sd, cloudflare-ips-v4.txt`) before running `harden-node.sh`. Refreshed by `../cf-sync.sh`, which also renders the AKS-side copies (`../ingress-nginx-service.yaml`, `../ingress-nginx-config.yaml`) and is run weekly by CI; never edit by hand. A stale list fails safe (the new edge is just refused, never wrongly trusted). |
| `gateway.yaml` | The workspace SSH gateway: one pod per pool node on the node's own `hostPort: 80`, behind the Cloudflare proxy (TLS ends at the edge). In its own `rustic-git-system` namespace, which the workspace NetworkPolicy names (`k8s::GATEWAY_NAMESPACE`). |
| `rotate-agent-token.sh` | Mint a new region agent token at the api and install it in the DaemonSet in one step. |
| `nix-conf.yaml` | ConfigMap: the host Nix daemon's substituters, keys and GC headroom. |
| `backup-controlplane.sh` | Hourly backup of the SQLite datastore, the cluster identity and a YAML dump of every CRD object to Azure Blob. Restore procedure is in the script's trailing comment. |
| `backup-controlplane.{service,timer}` | The systemd units that make "hourly" true — see "Control-plane backup" below. |

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
kubectl apply -f crds.yaml -f agent-rbac.yaml -f agent-admission.yaml -f nix-conf.yaml -f agent-daemonset.yaml -f gateway.yaml
```

### Cutover off PersistentVolumes

Pod volumes cannot be patched, so pods built against PVCs are deleted and recreated in the new
shape. After rolling the agent:

```sh
kubectl delete pods -A -l rustic-git.io/kind=workspace
kubectl delete pods -A -l rustic-git.io/kind=environment
kubectl delete pvc -A -l rustic-git.io/kind=volume
kubectl delete pv -l rustic-git.io/kind=volume
```

Each running workspace restarts once. Nothing on disk is touched: the subvolumes the PVs pointed at
are the same ones the pods now mount directly.

Nodes need labels before the DaemonSet will schedule and before placement will pick them:

```sh
kubectl label node <node> rustic-git.io/pool=true          # has a btrfs pool: run the controller here
kubectl label node <node> rustic-git.io/session=true       # may host workspaces
kubectl label node <node> rustic-git.io/env=true           # may host environments
```

One key per role, not `role=session`, because a label key holds one value and a small cluster needs
one node to be both.

## Control-plane backup

The CRDs are one SQLite file on one VM. `backup-controlplane.sh` copies it (plus the cluster
identity and a `kubectl get … -o yaml` of every object) to the `k3s-backup` container every hour,
but only once the timer is installed — this is the step that was missing. Once, on `k3s-cp`:

```sh
# 1. A SAS on container k3s-backup with create+write only (no list/delete: the script rotates by
#    overwriting fixed names, so a leaked SAS cannot read or destroy history), and a healthchecks-style
#    monitor URL with a 1 h period. Both by hand on the node:
ssh azureuser@<k3s-cp> 'sudo install -d -m700 /etc/rustic-git \
  && sudo sh -c "umask 077; cat > /etc/rustic-git/k3s-backup.sas" \
  && sudo sh -c "echo SNITCH_URL=https://hc-ping.com/<uuid> > /etc/rustic-git/k3s-backup.env"'
# 1b. The encryption key. The bundle carries the cluster CA, the SA signing key and the join
#     token, so it is encrypted before it leaves the node and the blobs are useless without this
#     file. Generate it once, then PUT A COPY IN THE PASSWORD MANAGER: a restore onto a fresh node
#     starts from the vault copy, and a lost key makes every backup noise.
ssh azureuser@<k3s-cp> 'sudo sh -c "umask 077; openssl rand -hex 32 > /etc/rustic-git/k3s-backup.key"; sudo cat /etc/rustic-git/k3s-backup.key'
# 2. Script and units.
scp deploy/k3s/backup-controlplane.{sh,service,timer} azureuser@<k3s-cp>:/tmp/
ssh azureuser@<k3s-cp> 'sudo install -m755 /tmp/backup-controlplane.sh /usr/local/bin/ \
  && sudo install -m644 /tmp/backup-controlplane.service /tmp/backup-controlplane.timer /etc/systemd/system/ \
  && sudo systemctl daemon-reload && sudo systemctl enable --now backup-controlplane.timer \
  && sudo systemctl start backup-controlplane.service && sudo systemctl status backup-controlplane.service --no-pager'
```

Reading it: `systemctl list-timers backup-controlplane.timer` shows the next and last run;
`journalctl -u backup-controlplane` the "backed up N bytes" lines; and
`az storage blob list -c k3s-backup --query '[].{n:name,t:properties.lastModified}' -o table`
the truth — the newest `hourly-*.tgz.enc` must be under two hours old. The snitch is the alert: it pages
when the hourly ping is *missing*, which is the failure a timer produces (a node off, a unit
disabled by an upgrade), and gets `/fail` when the run itself fails. The unit fails — and says why
in the journal — if the API server was down for the CRD dump, even though `state.db` still went
up. The account defaults to `rusticgitkolomi`; override `ACCOUNT`/`CONTAINER` in the `.env` file.
Retention, what it does and does not cover, and the Azure-side switches for everything else are in
`deploy/BACKUPS.md`.

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
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-rbac.yaml -f deploy/k3s/agent-admission.yaml -f deploy/k3s/api-rbac.yaml

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

# 6. Only then the API tier, on AKS (deploy/rustic-git.yaml, pinned to CI's SHA by deploy/pin.sh;
#    deploy/roll.sh applies it in one go).
deploy/roll.sh
kubectl rollout status deploy/rustic-git-api -n rustic-git
```

Then verify by hand what `tests/ws_e2e.sh`'s seeded phase proves — nightly in
`.github/workflows/e2e.yml`'s `workspaces` job once a `btrfs-k3s` self-hosted runner is
registered on the build VM, and by hand until then: use "Open in a workspace"
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
   `kubectl -n rustic-git-system create secret tls gateway-tls --cert=<cert> --key=<key>`.
4. Copy the `rustic-git-jwt` Secret from AKS into this cluster's `rustic-git-system` (the gateway
   verifies session tokens locally, with the same secret the api mints them with), and check
   the `WS_REGION` in `gateway.yaml`'s ConfigMap equals the agent Secret's. Moving the gateway
   out of `kube-system` (2026-08-29) is a roll in this order: the agent first (its next reconcile
   rewrites every workspace's `allow-gateway-ssh` policy to the new namespace), then
   `kubectl apply -f gateway.yaml`, then `kubectl -n kube-system delete deploy,svc,sa
   rustic-git-gateway`. SSH sessions drop once, between the agent roll and the new gateway
   coming up.
5. The Azure NSG in front of the pool nodes (`k3s-nsg`, resource group `rustic-git-k3s`) needs the
   same admission — it sits before nftables and drops 80 otherwise. One rule, TCP 80 from
   Cloudflare's v4 ranges (the list in `cloudflare-ips-v4.txt`, spelled out as separate prefixes):
   `az network nsg rule create -g rustic-git-k3s --nsg-name k3s-nsg -n gateway-cloudflare
   --priority 120 --direction Inbound --access Allow --protocol Tcp --destination-port-ranges 80
   --source-address-prefixes <cidr> <cidr> …`. Not created by `provision-azure.sh`; when the
   list changes, `../cf-sync.sh` prints the matching `az network nsg rule update` — run it.
6. `harden-node.sh` on each pool node so the node's 80 admits only Cloudflare's edge. The script
   is streamed over ssh (`sudo bash -s <`), so it has no file of its own on the remote box to read
   a CIDR list from — build `CF_CIDRS` locally and pass it as an env var on the remote command:

   ```sh
   CF_CIDRS="$(paste -sd, deploy/k3s/cloudflare-ips-v4.txt)"
   ssh azureuser@<node> "sudo CF_CIDRS='$CF_CIDRS' ADMIN_CIDR='$ADMIN_CIDR' API_CLIENTS='$API_CLIENTS' bash -s" \
     < deploy/k3s/harden-node.sh
   ```

## Two things that bite

**The agent Secret is not in this directory.** `rustic-git-agent` in `kube-system` carries the
region's agent token and the Azure credentials, and it is created by hand because it holds secrets.
Without it the controller runs but every push fails at the registry. Keys: `WS_REGISTRY_URL`,
`WS_REGION`, `WS_AGENT_TOKEN`, `AZURE_ACCOUNT`, `AZURE_KEY`, `AZURE_CONTAINER`.

**A snapshot can only be restored in the region it was pushed in.** A `CommitRecord` names the
region its blobs live in, and a `restoreOf` source carries that region down to the agent. The
agent's `AZURE_*` triple points at ITS region's container and there is no way to give it another
one, so a restore naming a different region fails immediately and by name
(`Ready=False/RegionUnreachable`), rather than sitting in `phase: working` forever, which is what
it used to do.

To move a workspace or an environment baseline between regions, push it again from a node in the
destination region; there is no cross-region read path.

**Do not run `tests/ws_e2e.sh` on a node the DaemonSet is running on.** The script starts its own
agent against its own loopback pool; two controllers reconciling one object materialize it into two
different pools. The script refuses to start if it sees the DaemonSet on its node — take the label
off first (`kubectl label node <node> rustic-git.io/pool-`) and put it back after.
