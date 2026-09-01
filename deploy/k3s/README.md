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
| `workspace-admission.yaml` | The ValidatingAdmissionPolicy that puts PSA `baseline`'s refusals back for workspace/environment pods (`hostNetwork`/`hostPID`/`hostIPC`, privileged containers, stray `hostPath` sources) now that the namespace floor is `privileged`. Matches on namespace, not identity — safe to apply any time, even before an agent rollout. |
| `agent-daemonset.yaml` | The node controller itself, one pod per pooled node. |
| `agent-peer.yaml` | NetworkPolicy admitting the replication listener (port 8444) only from other agent pods. No Service — discovery is by pod IP from the API. See "Replication" below. |
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

On a **fresh cluster** — nothing running yet, so none of the ordering below applies — apply
everything in one command:

```sh
kubectl apply -f crds.yaml -f agent-rbac.yaml -f agent-admission.yaml -f workspace-admission.yaml -f nix-conf.yaml -f agent-daemonset.yaml -f agent-peer.yaml -f gateway.yaml
```

### Upgrading an existing cluster off PersistentVolumes

`agent-rbac.yaml`'s ClusterRole no longer grants the agent `get`/`list`/etc. on `PersistentVolume`
— the hostPath rework deleted that need. Applying the new RBAC and the new DaemonSet together is
**unsafe**: the ClusterRole is cluster-scoped and takes effect the instant it's applied, but a
DaemonSet rollout is not instant. An old agent pod still running on a not-yet-rolled node calls
`get` on a PV as part of its reconcile, gets back 403 instead of the old behavior, and its **entire
reconcile pass aborts** — every workspace on that node stops converging until that node's pod is
replaced. Apply in this order:

1. Pin and apply `agent-daemonset.yaml` only, and **wait for the rollout to finish on every node**
   (`kubectl rollout status daemonset/rustic-git-agent -n <ns>`) before moving on.
2. Only then apply `agent-rbac.yaml` (and `agent-admission.yaml`).
   `workspace-admission.yaml` is the exception to this ordering: it is deny-only, matches on
   namespace rather than agent identity, and requires nothing from the new agent, so apply it
   whenever convenient — before step 1 is fine too.
3. Then run the cutover deletes below: pods, then pvc, then pv.

```sh
kubectl delete pods -A -l rustic-git.io/kind=workspace
kubectl delete pods -A -l rustic-git.io/kind=environment
kubectl delete pvc -A -l rustic-git.io/kind=volume
kubectl delete pv -l rustic-git.io/kind=volume
```

Each running workspace restarts once. Nothing on disk is touched: the subvolumes the PVs pointed at
are the same ones the pods now mount directly.

**The namespace PSA label also flaps during this window.** `ws-{owner}` is shared across nodes and
every node's binding reconciler applies the PSA label it believes is correct: a new (rolled) agent
stamps `privileged`, while an old agent on a node not yet rolled keeps re-applying `baseline`. A
hostPath pod scheduled into that namespace during the flap can be refused by whichever label won
the last write. This is self-healing once the last old agent pod is gone — but until then,
workspace creation across the cluster is unreliable, not just on the unrolled nodes, so don't rely
on it for anything user-facing mid-rollout.

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
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-rbac.yaml -f deploy/k3s/agent-admission.yaml -f deploy/k3s/workspace-admission.yaml -f deploy/k3s/api-rbac.yaml

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

## Release: the commit model

The 2026-09-01 change: `push` stops uploading a whole-tree delta to the object store and starts
cutting a local btrfs commit (a `Snapshot` CR, `snap/{name}` under the volume) that replicates to
other nodes as `VolumeReplica` rows — `crates/workspaces/src/engine/commit.rs`'s module doc and
`crates/workspaces/src/crd.rs`'s `Snapshot`/`VolumeReplica` kinds are the design. This was gated
behind `WS_COMMIT_MODEL=1` through the cutover; as of Task 8 the flag and the object-store path it
guarded are both deleted outright — the commit model is the only model, unconditionally, in every
build from here on. **The kill switch described below is therefore gone too: past this point,
rollback to the object-store push path is a VERSION rollback (redeploy an older image), not a
config flip** — there is no runtime toggle to catch a bad rollout, so verify a commit-model change
on a canary node before rolling the fleet.

Order:

```sh
# 1. CRDs first — the new kinds (Snapshot, VolumeReplica) and the selectableFields the agent's
#    watches filter on. Already applied on dev; harmless to re-apply.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/crds.yaml

# 2. Roll the agent (repin the image tag to the SHA CI built, then apply and wait).
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-daemonset.yaml
KUBECONFIG=.local/k3s.yaml kubectl rollout status ds/rustic-git-agent -n kube-system

# 3. Then the API tier (rustic-git.yaml) — deploy/roll.sh applies it.
deploy/roll.sh
kubectl rollout status deploy/rustic-git-api -n rustic-git
```

**Every existing volume's pod restarts once — and, for homes, force it explicitly.** There is no
bulk migration step: an old-layout volume (`{pool}/vol/{id}/live` is itself the RW subvolume) is
migrated the first time it's CLAIMED on a node under the flag (`migrate_and_seed_baseline` in
`bins/agent/src/controller.rs`, calling `Engine::migrate_volume`), which moves `live` to a
directory holding one worktree, `live/{id}` — and the pod that was mounting the OLD path has to be
recreated to pick up the new one, exactly like the hostpath cutover above. A workspace/environment
volume migrates lazily, the first time it's claimed, so its pod restarting on its own schedule is
enough. **A home does not wait for a claim: the home beat migrates every ready home on every pass
(H3), so its already-running pod's hostPath (`home_volume` in `crates/workspaces/src/k8s.rs`) goes
stale on the FIRST post-rollout beat** — `ensure`'s Server-Side Apply then 422s against that pod's
immutable `hostPath` until it's deleted. Same step as the hostpath cutover's Step 3, run right
after step 2 above:

    kubectl delete pods -A -l rustic-git.io/kind=workspace
    kubectl delete pods -A -l rustic-git.io/kind=environment

Nothing forces every volume to migrate at once, and nothing needs to — the delete above just
closes the 422 window for homes; a workspace/environment volume's own pod recreates on whatever
schedule already recycles it.

Verify:

```sh
kubectl get snapshots                          # Ready rows appear as workspaces push
kubectl get volumereplicas                      # Synced on every node the volume replicates to
kubectl get workspace <id> -o jsonpath='{.status.head}'   # a commit name once it's pushed or migrated
```

**There is no kill switch any more (Task 8) — this is the sentence that matters.** Before Task 8,
unsetting `WS_COMMIT_MODEL` (or setting it back to `"0"`) rolled back cleanly ONLY for a volume
that had never migrated; a volume that HAD migrated (`live` moved from the single old subvolume to
the `live/{id}` worktree directory) could not roll back at all without manually undoing the move —
old code mounting `{pool}/vol/{id}/live` verbatim on a migrated volume mounts the directory
*containing* the worktree, not the worktree itself, which is wrong data, not a clean failure. That
whole flag-off code path is deleted now, so there is nothing left to flip back to: every volume is
on the commit-model layout (migrated lazily, the first time it's claimed on a node, same as
before), and the ONLY way back is a version rollback to a pre-Task-8 image, which then needs the
exact inverse of `Engine::migrate_volume` run by hand on any volume that has since migrated:

```sh
# On the node holding the volume, with its pod stopped, running a pre-Task-8 image:
mv {pool}/vol/{id}/live/{id} {pool}/vol/{id}/live-migrating
rmdir {pool}/vol/{id}/live
mv {pool}/vol/{id}/live-migrating {pool}/vol/{id}/live
```

There is no bulk "undo everything" command, deliberately — the same one-volume-at-a-time shape as
the forward migration.

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

## Replication

Volume/Workspace/Environment standbys, off by default. Bring it up:

1. Add `WS_PEER_SECRET` to the `rustic-git-agent` Secret (same one as `WS_REGION` etc. above) —
   any shared string, compared in constant time on every peer request. Unset, the peer listener
   never starts and the sender beat never runs: fail-closed, not fail-open.
2. Apply `agent-peer.yaml` (already in the fresh-cluster command above) so the listener's port
   8444 is reachable only from other `app: rustic-git-agent` pods — without it, every pod in the
   cluster, workspace pods included, can already reach an agent pod's IP directly.
3. Raise `WS_REPLICA_COUNT` in `agent-daemonset.yaml` above `1` (and roll) once the secret is
   live on every node.

Rollout order between these three is unconstrained: `WS_REPLICA_COUNT` defaults to `1` (no
standby, no listener call, no snapshot), and the listener itself is fail-closed without its
secret, so an agent that rolls ahead of its peers, or ahead of the Secret, or ahead of
`agent-peer.yaml`, just keeps running with replication off rather than sending or receiving
anything unsafe.
