# The k3s workload cluster

Workspaces and environments run here. The git/registry server tier does not — it stays on its own
cluster and this one reaches it over HTTP.

Files, in the order a cluster is built:

| File | What it does |
| --- | --- |
| `env.example.sh` | Copy to `env.sh` (git-ignored) and edit. Sizes, names, operator CIDR, `API_CLIENTS` (the api tier's egress, admitted to 6443 by both the NSG and nftables), `CF_IPS_FILE` (defaults to `cloudflare-ips-v4.txt`). |
| `provision-azure.sh` | VNet, NSG (including the Cloudflare-on-80 and api-tier-on-6443 rules, from `CF_IPS_FILE`/`API_CLIENTS`), control plane, workers, build VM. Idempotent — re-run after a partial failure. |
| `format-pool.sh` | Run on each worker with the data disk as argument. btrfs at `/wspool-prod`. |
| `crds.yaml` | **Generated** — do not hand-edit. `CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml`. |
| `agent-rbac.yaml` | ServiceAccount + ClusterRole for the node controller. The header table is the role: one row per call the agent makes. |
| `agent-admission.yaml` | The ValidatingAdmissionPolicy that makes the role true — refuses the agent any spec write but `Volume.spec.restoreTo`, and pins every namespaced object it writes — pods, statefulsets, services, networkpolicies, limitranges, Secrets, RoleBindings — to the `ws-`/`wt-`/`env-` namespaces it makes. Apply with `agent-rbac.yaml`, always. |
| `workspace-admission.yaml` | The ValidatingAdmissionPolicy that puts PSA `baseline`'s refusals back for workspace/environment pods (`hostNetwork`/`hostPID`/`hostIPC`, privileged containers, stray `hostPath` sources) now that the namespace floor is `privileged`. Matches on namespace, not identity — safe to apply any time, even before an agent rollout. |
| `system-netpol.yaml` | The one NetworkPolicy admitting 2049 to ZeroFS. The mount runs in the node's netns via `nsenter`, so its client is the node, never an agent pod: the `from` list is the node subnet (`10.60.1.0/24`, for a mount on ZeroFS's own node) **plus one `/32` per node's `flannel.1` address** (every other node, masqueraded over VXLAN). Hand-maintained — a new node's `/32` goes in before its agent mounts. The export authenticates nothing, so reachability is the authorization. `app: zerofs` is the sole selector. Apply with `zerofs.yaml`. |
| `agent-daemonset.yaml` | The node controller itself, one pod per pooled node. |
| `zerofs.yaml` | The region's shared-home NFS export (ZeroFS, single replica — see its header). Apply before rolling agents with `WS_HOMES_EXPORT` set; see "Shared home" below. |
| `agent-peer.yaml` | NetworkPolicy admitting the replication listener (port 8444) only from other agent pods, and metrics (9464) only from a namespace literally named `monitoring` — no such namespace exists yet, so edit that rule to name the scraper's real namespace before expecting 9464 to be reachable. No Service — discovery is by pod IP from the API. See "Replication" below. |
| `harden-node.sh` | Node firewall (drop-by-default on the public NIC), unattended upgrades, keys-only sshd. Idempotent; run on every node after provisioning and after changing the operator CIDR, and again with `CF_CIDRS` set once the gateway is live. Streamed over `ssh … sudo bash -s < harden-node.sh`, so `CF_CIDRS` must be passed as an env var on the remote command, not read from a local file — see the Gateway section below. |
| `cloudflare-ips-v4.txt` | Cloudflare's published v4 edge ranges, one CIDR per line — the one source. Build `CF_CIDRS` from it locally (`paste -sd, cloudflare-ips-v4.txt`) before running `harden-node.sh`. Refreshed by `../cf-sync.sh`, which also renders the AKS-side copies (`../ingress-nginx-service.yaml`, `../ingress-nginx-config.yaml`) and is run weekly by CI; never edit by hand. A stale list fails safe (the new edge is just refused, never wrongly trusted). |
| `gateway.yaml` | The workspace SSH gateway: one pod per pool node on the node's own `hostPort: 80`, behind the Cloudflare proxy (TLS ends at the edge). In its own `rustic-git-system` namespace, which the workspace NetworkPolicy names (`k8s::GATEWAY_NAMESPACE`). |
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
kubectl apply -f crds.yaml -f agent-rbac.yaml -f agent-admission.yaml -f workspace-admission.yaml -f nix-conf.yaml -f agent-daemonset.yaml -f agent-peer.yaml -f gateway.yaml -f system-netpol.yaml
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

A new node ALSO needs its `flannel.1` address added to `system-netpol.yaml` before its agent first
mounts the shared home — that policy's `from` list is hand-maintained, and an agent whose mount is
denied wedges on a `hard` mount rather than failing:

```sh
ssh <node> ip -4 -o addr show flannel.1     # e.g. 10.42.6.0 — add `- ipBlock: {cidr: 10.42.6.0/32}`
kubectl apply -f system-netpol.yaml
```

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

Restoring: every `k3s-backup.tgz.enc` blob travels with a detached `k3s-backup.tgz.enc.hmac`,
because AES-CBC is unauthenticated — a truncated or tampered blob decrypts to garbage rather than
failing. Download both, recompute the HMAC over the downloaded `.enc` with the same key
(`openssl dgst -sha256 -mac HMAC -macopt "hexkey:$(od -An -tx1 < k3s-backup.key | tr -d ' \n')" -r ... | cut -d' ' -f1`), `diff` it
against the downloaded `.hmac`, and only decrypt (`openssl enc -d ...`) once they match. A mismatch
means do not restore — fetch an older `hourly-*`/`daily-*` slot instead. Full restore steps are the
comment block at the bottom of `backup-controlplane.sh`.

## Rotating the api tier's kubeconfig

The api tier (`bins/api`) authenticates to this cluster with a long-lived kubeconfig held in the
`rustic-git-k3s-kubeconfig` Secret, mounted at `/etc/rustic-git/k3s` on the `rustic-git-api`
Deployment (`deploy/rustic-git.yaml`) and pointed at by `KUBECONFIG` there. It is a stopgap — the
`ponytail:` note beside that env var says the pull design in
`docs/superpowers/specs/2026-08-26-cluster-sync-design.md` replaces it with each cluster syncing
its own desired state, at which point this Secret goes away entirely. Until then, rotate it:

```sh
# 1. A fresh bound token for the api's own ServiceAccount (deploy/k3s/api-rbac.yaml), scoped to
#    this cluster only — nothing wider than what /v1 already had.
KUBECONFIG=.local/k3s.yaml kubectl -n kube-system create token rustic-git-api --duration=8760h > /tmp/api.token

# 2. Build a kubeconfig around it with the cluster's CA (already on disk from provisioning, or
#    pull it fresh):
KUBECONFIG=.local/k3s.yaml kubectl config view --raw --minify -o jsonpath='{.clusters[0].cluster.certificate-authority-data}' \
  | base64 -d > /tmp/api-ca.crt
API_SERVER=$(KUBECONFIG=.local/k3s.yaml kubectl config view --raw --minify -o jsonpath='{.clusters[0].cluster.server}')
kubectl config set-cluster k3s --server="$API_SERVER" --certificate-authority=/tmp/api-ca.crt --embed-certs=true --kubeconfig=/tmp/api.kubeconfig
kubectl config set-credentials rustic-git-api --token="$(cat /tmp/api.token)" --kubeconfig=/tmp/api.kubeconfig
kubectl config set-context default --cluster=k3s --user=rustic-git-api --kubeconfig=/tmp/api.kubeconfig
kubectl config use-context default --kubeconfig=/tmp/api.kubeconfig

# 3. Replace the Secret in the AKS cluster (default context — this is a different cluster from
#    the k3s one above), and roll the api pods to pick it up:
kubectl create secret generic rustic-git-k3s-kubeconfig --from-file=config=/tmp/api.kubeconfig \
  --dry-run=client -o yaml | kubectl -n rustic-git apply -f -
kubectl -n rustic-git rollout restart deploy/rustic-git-api

# 4. Verify with a live read through the new token, then let the old token expire — a bound token
#    has no separate revoke call, so "the old token stops working" means deleting the Secret entry
#    is not enough; the old token is only dead once its --duration expires or the ServiceAccount
#    it was bound to is deleted and recreated. Rotating on the schedule below is what keeps that
#    window short.
curl -s https://api.rustic-git.example/v1/regions -H "Authorization: Bearer <api client token>" | head -c 200

# 5. Clean up the local token files — they are as sensitive as the kubeconfig itself.
rm -f /tmp/api.token /tmp/api-ca.crt /tmp/api.kubeconfig
```

Cadence: yearly (the `--duration=8760h` above), or immediately on any suspicion the token leaked —
a laptop with `.local/k3s.yaml` on it going missing, a log line with the token in it, anything of
that shape.

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
#    watches filter on. Already applied on dev; harmless to re-apply. This now includes
#    VolumeReplica's `.spec.volume` selector (added 2026-09-02, used by `flush_gate` and
#    `pull_volume`): apply it BEFORE the agent image that reads it — an agent ahead of the CRD
#    gets a 400 on every stop's flush gate.
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

### Old-model artifact cleanup — irreversible, gated on the verify above

Only after EVERY volume shows a Synced VolumeReplica on its replica set and a `status.head`, and a
few days of real pushes have proven the model (the Azure blobs are the last copies of the OLD
history — deleting them is the point of no return for restore-from-blob):

```sh
# on each pooled node (agent pod shell), the old model's pool artifacts:
rm -rf /wspool-prod/recv /wspool-prod/stage /wspool-prod/img
rm -f  /wspool-prod/vol/*.lineage /wspool-prod/vol/*.pushed-gen        /wspool-prod/vol/*.replicated-gen-* /wspool-prod/vol/*/.pushed-gen
# the old peer-replication staging (superseded by snap/ under each volume):
rm -rf /wspool-prod/repl

# the object store (LAST — nothing reads these after the cutover, and nothing can rebuild them):
az storage blob delete-batch --account-name <acct> -s <container> --pattern 'layers/*'
```

The `bins/server/src/browse_api/volumes.rs` browse surface stays up through all of the above: it
is frozen (nothing writes it any more) but the Snapshots page still reads it, and it keeps showing
old-model history for a volume until that volume's rows age out or the surface itself is retired.

The `rustic-git-agent` Secret's `AZURE_*` keys become unused (the env wiring is already gone); the
keys may stay in the Secret harmlessly or be pruned with the storage account whenever the old
container is retired.


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

## Release: shared home on ZeroFS

The 2026-09-01 change after the commit model: `/home/kl` stops being ONE btrfs subvolume per
owner per node (`home-{owner}`, migrated lazily like a workspace, pushed on a timer and on every
stop) and becomes ONE region-shared NFS export, served by ZeroFS (`deploy/k3s/zerofs.yaml`,
SlateDB-backed, single replica on purpose — see its header) and mounted once per node by the
agent at `{pool}/homes` (`WS_HOMES_EXPORT`, `bins/agent/src/lib.rs`'s `run`). A pod hostPaths
`{pool}/homes/{owner}` straight onto `/home/kl`; caches/editor-servers/shell-state stay node-local
(`{pool}/homecache/{owner}`, subPaths `cache`/`vscode-server`/`cursor-server`/`state`, listed in
`crates/workspaces/src/k8s.rs`'s `workspace_pod`). There is no home Volume any more, so the
owner→node pin that came from it is also gone: an owner's workspaces can be claimed by any node
whose `VolumeReplica` is Synced, not just the one that happened to hold their btrfs home.

Order:

```sh
# 1. Real credentials before anything else. ZeroFS speaks Azure Blob NATIVELY (`azure://`) — there
#    is no S3 gateway and no AWS_* anywhere — so it points at the SAME account and container every
#    other tier already writes to. Reuse the region's existing storage credential rather than
#    minting a second one; only the encryption password is new, and losing it loses every home in
#    the region (extents are encrypted with a key derived from it). Back the password up wherever
#    the region's other secrets are backed up — the cluster itself is not such a place.
ACC=$(kubectl -n rustic-git get secret rustic-git-storage -o jsonpath='{.data.account}' | base64 -d)
KEY=$(kubectl -n rustic-git get secret rustic-git-storage -o jsonpath='{.data.key}' | base64 -d)
kubectl -n rustic-git-system create secret generic zerofs-store \
  --from-literal=AZURE_STORAGE_ACCOUNT_NAME="$ACC" \
  --from-literal=AZURE_STORAGE_ACCOUNT_KEY="$KEY" \
  --from-literal=ZEROFS_CONTAINER=rustic-git \
  --from-literal=ZEROFS_PREFIX=homes \
  --from-literal=ZEROFS_PASSWORD="$(openssl rand -base64 32)"   # RECORD THIS SOMEWHERE DURABLE
# The Secret is NOT in zerofs.yaml on purpose: a manifest re-applied on every rollout must not
# carry secret material, or `kubectl apply -f zerofs.yaml` overwrites the real credentials with
# placeholders and ZeroFS crash-loops on `Invalid Access Key` with nothing in the diff to explain
# it. Create it here, once, and let the apply below touch only the ConfigMap/Deployment/Service.
kubectl apply -f deploy/k3s/zerofs.yaml
kubectl rollout status deploy/zerofs -n rustic-git-system

# 2. Add WS_HOMES_EXPORT to the agent DaemonSet (deploy/k3s/agent-daemonset.yaml already carries
#    it, pointed at the Service above) and roll the agents. Each one mounts {pool}/homes at
#    startup, before the controller starts — a failed mount fails agent startup on purpose (the
#    DaemonSet restart-loops rather than a node silently serving an empty home).
kubectl apply -f deploy/k3s/agent-daemonset.yaml
kubectl rollout status ds/rustic-git-agent -n kube-system
```

Note the agents roll BEFORE step 3 copies anything, so a workspace pod recreated in this window
mounts the new, still-empty export: a person may briefly see an empty home. Nothing is lost —
step 3's `rsync -a` merges the old content in afterwards — but it is worth doing outside a busy
hour, and worth knowing before the first "my home is gone" message arrives.

**3. Migrate, per owner that has a `home-{owner}` Volume — one owner at a time, same shape as
every other migration in this file. The two paths below are NEVER typed by hand: the source
directory name is lowercased and can be truncated (`home_volume_name` → `dns_label`, `crd.rs`),
the destination is the owner's handle verbatim — original case, never truncated
(`ensure_shared_home`/`homes_root` in `controller.rs`, called straight off `spec.owner`). For any
owner whose handle has uppercase, or is long enough to truncate, these are DIFFERENT strings.
Typing one `<owner>` value consistently (lowercase, since that's the one that makes the source
directory exist) rsyncs into a sibling directory the pods never mount — the person logs into an
empty home, silently, and step 5 later deletes the real data. Read both off the cluster instead:**

```sh
# Source: the Volume's own name IS the directory name — read it, never reconstruct it. (A
# truncated long-handle home — dns_label kicks in past 63 chars — still matches this grep, since
# the "home-" prefix always survives truncation; see step 5's note on the same blind spot.)
for HOME_VOL in $(kubectl get volumes --no-headers | awk '{print $1}' | grep '^home-'); do

  # Destination: spec.owner, straight off the Volume — original case, exactly what
  # ensure_shared_home mounts pods onto. Never re-lowercase or re-derive this from $HOME_VOL.
  OWNER=$(kubectl get volume "$HOME_VOL" -o jsonpath='{.spec.owner}')
  # $OWNER goes into the jsonpath filter unescaped: a handle containing a double quote would
  # break the expression into one that silently matches nothing rather than erroring, and the
  # loop would then "migrate" an owner with zero workspaces stopped. Sanity-check WS_IDS below.
  WS_IDS=$(kubectl get workspaces -o jsonpath="{.items[?(@.spec.owner==\"$OWNER\")].metadata.name}")

  # Stop every workspace of the owner's — jsonpath, not `-o name` (which prints
  # `workspace.rustic-git.io/<id>`, and a curl built from THAT 404s under `-f`):
  for id in $WS_IDS; do
    curl -fsS -X POST "$API_BASE/v1/workspaces/$id/stop" -H "Authorization: Bearer $ADMIN_TOKEN"
  done

  # On the node the OwnerBinding pinned (`kubectl get volume $HOME_VOL -o
  # jsonpath='{.status.nodeName}'`), copy the old home's content into the new export, EXCLUDING
  # the six node-local cache dirs listed on the rsync below (they are NOT what the pod mounts —
  # the running layout redirects caches by env var, `login_env` -> HOME_CACHE_DIR, onto four
  # hardcoded homecache subPaths; cross-check `workspace_pod` for that) — they were nested
  # btrfs subvolumes the old design never pushed, and copying them across is dead weight the new node-local
  # {pool}/homecache/{owner} rebuilds for free on first use anyway. The old worktree is
  # {pool}/vol/{HOME_VOL}/live/{HOME_VOL} — NOT {pool}/vol/{HOME_VOL}/live, which is the directory
  # the worktree sits inside, not the worktree itself (same trap the commit-model migration above
  # calls out). TRAILING SLASHES MATTER: both paths below end in `/`, which means "copy the
  # CONTENTS of the left directory into the right one" — drop either trailing slash and rsync
  # nests the whole tree one level deeper instead of landing it at the mount root.
  sudo rsync -a \
    --exclude='.cache' --exclude='.npm' --exclude='.cargo/registry' \
    --exclude='.local/share/pnpm' --exclude='.vscode-server' --exclude='.cursor-server' \
    "/wspool-prod/vol/$HOME_VOL/live/$HOME_VOL/" "/wspool-prod/homes/$OWNER/"

  # Restart the owner's workspaces; their pods now hostPath the NFS export instead of the old
  # subvolume.
  for id in $WS_IDS; do
    curl -fsS -X POST "$API_BASE/v1/workspaces/$id/start" -H "Authorization: Bearer $ADMIN_TOKEN"
  done
done
```

```sh
# 4. The narrowed admission policy — the agent no longer writes Volume.spec.quotaGb (there is no
#    home Volume, no qgroup, left for it to write it onto), so the ONE spec field it may still
#    touch is Volume.spec.restoreTo. Apply once every owner that needs to keep running through the
#    migration has been moved (an agent running the old policy alongside a rolled-forward agent
#    binary is harmless either order, but the policy is what makes the RBAC table true, so don't
#    leave it stale for long):
kubectl apply -f deploy/k3s/agent-admission.yaml
```

**5. Days later, irreversible — delete each `home-{owner}` Volume CR, same warning shape as the
old-model artifact cleanup above:**

Only after EVERY migrated owner has been running on the shared export for a few days with no
regressions reported (this is the point of no return — the Volume's finalizer reclaims its btrfs
subvolume on delete, and once that subvolume is gone the pre-migration home content is
UNRECOVERABLE, there is no second copy anywhere):

```sh
# Same `grep '^home-'` step 3 uses to find these — the "home-" prefix survives `dns_label`
# truncation for any realistic owner handle, but if a home Volume for a very long or unusual
# handle is ever unaccounted for here, list every Volume and check its ownerReference kind
# (OwnerBinding) rather than trusting the name pattern alone.
kubectl get volumes -l rustic-git.io/owner --no-headers | awk '{print $1}' | grep '^home-' \
  | xargs -r -n1 kubectl delete volume
```

The home-Volume-specific code (`is_home_volume`, `home_volume_name`, the OwnerBinding's
`ensure_home`) is already gone from the binary that ships this migration — nothing after step 2
above ever creates a `home-*` Volume again, which is why this step is cleanup of what earlier
owners accumulate, not a code change.

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
5. The Azure NSG in front of the pool nodes needs the same admission — it sits before nftables and
   drops 80 otherwise. Both the `allow-http-cloudflare` and `allow-apiserver-api-tier` NSG rules
   are created by `provision-azure.sh`; the NSG and nftables are two layers of the same list, and
   neither is edited by hand.
6. `harden-node.sh` on each pool node so the node's 80 admits only Cloudflare's edge. The script
   is streamed over ssh (`sudo bash -s <`), so it has no file of its own on the remote box to read
   a CIDR list from — build `CF_CIDRS` locally and pass it as an env var on the remote command:

   ```sh
   CF_CIDRS="$(paste -sd, deploy/k3s/cloudflare-ips-v4.txt)"
   ssh azureuser@<node> "sudo CF_CIDRS='$CF_CIDRS' ADMIN_CIDR='$ADMIN_CIDR' API_CLIENTS='$API_CLIENTS' bash -s" \
     < deploy/k3s/harden-node.sh
   ```

## Two things that bite

**The agent Secret is not in this directory.** `rustic-git-agent` in `kube-system` carries
`WS_REGION` and `WS_PEER_SECRET`, and it is created by hand because it holds a secret. The agent
holds no registry URL, no agent token and no Azure credential any more: its commit history is the
`Volume`'s own status.

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

Volume standbys, pull-based: every node's agent decides which commits it must hold (replication's
rendezvous names it, or it runs one of the volume's worktrees) and GETs them from a peer that
already has them. Bring it up:

1. Add `WS_PEER_SECRET` to the `rustic-git-agent` Secret (same one as `WS_REGION` etc. above) —
   any shared string, compared in constant time on every peer request. Unset, the peer listener
   never starts and the puller never dials: fail-closed, not fail-open.
2. Apply `agent-peer.yaml` (already in the fresh-cluster command above) so the listener's port
   8444 is reachable only from other `app: rustic-git-agent` pods — without it, every pod in the
   cluster, workspace pods included, can already reach an agent pod's IP directly.

How many copies a volume gets is `Volume.spec.replicas`, per volume — there is no cluster-wide
knob. Rollout order is unconstrained: the listener and the puller are both fail-closed without the
secret, so an agent that rolls ahead of its peers, or ahead of the Secret, or ahead of
`agent-peer.yaml`, just keeps running with replication idle rather than moving anything unsafe.

### Sync points

`kubectl get snapshots` shows more than commits: `sync-{worktree}-{8hex}` is a transient, cut by
the sync beat from a running worktree's moved btrfs generation so a peer has something recent to
pull between pushes; `stop-{ws}-{gen}`/`stop-{env}-{gen}` is the same mechanism cutting one last transient on
the way down. There is at most one Ready transient per worktree at a time — retain deletes the
previous one only once the new one is Ready, so seeing two Ready together is a brief overlap, not
a leak. `WS_SYNC_SECS` (default 60) is how often the beat checks; `WS_STOP_FLUSH_TIMEOUT_SECS`
(default 600) is how long a stop waits for a peer's `VolumeReplica` to pick up the stop transient
before giving up. Condition reason `FlushUnreplicated` on a stopped workspace or environment means
exactly that: nothing synced the final transient in time, so the stop went through unreplicated —
check whether the volume has any peers at all (`spec.replicas`) and whether replication is even
enabled (`WS_PEER_SECRET`, above) before assuming the timeout is too short. On a SINGLE-NODE region
the gate is skipped entirely rather than waited out — there is nowhere for the bytes to go, so
every stop lands at once with message "no other node in the region holds replicas".

### Node death

A node is dead once its `Node` object has been NotReady for `WS_NODE_DEAD_SECS` (code default 600; the DaemonSet sets 180, enough for a reboot to finish without healing around it) —
that floor, not a shorter probe, is what keeps a brief kubelet hiccup from tearing anything down.
Every surviving agent's pull beat then does two things to that node's volumes: replicas heal onto
a third live node automatically (rendezvous stops naming the dead node as a candidate, so a
survivor picks up the missing copy on its own), and every `Volume` the dead node owned goes
`status.phase: Unavailable` with condition `Available=False/NodeDead` — `kubectl get volumes`
is the fast way to see which ones are affected.

What moves automatically stops there. A Workspace or Environment whose `desiredState` is
`Stopped` is un-placed by the sweep, its volume's owner pin cleared, and a survivor with a Synced
replica claims it on the next beat — this is the whole re-hosting path, no data lost, because a
stopped worktree was already flushed and replica-gated when it stopped. A RUNNING one is left
alone: its pin stays on the dead node and it gets condition `Degraded=True/NodeDead`, because its
live edits since the last sync point exist only on that node's disk, and re-hosting it
automatically would silently throw them away. The `/v1` stop endpoint reports that cost as a
`warning` in its response body when it sees the condition, so stopping (and thereby moving) a
workspace on a dead node is always something a person chose, never something the sweep did for
them. If the node comes back before anyone stops it, its pin was never cleared, so nothing
moves — the pod resumes with everything intact.

Caveat: this all keys off "NotReady in the API server for `WS_NODE_DEAD_SECS`", which is not the
same fact as "the node's pods stopped running". A node that is merely network-partitioned from
the control plane keeps its Running pods alive and keeps writing to their volumes for as long as
the partition lasts; the sweep on every other node still marks its volumes `Unavailable` and, for
any workspace already `Stopped`, still hands the volume to a survivor. Two writers can therefore
exist briefly across a partition, which is why the admission policy only ever allows the two
scripted `nodeName` transitions (an empty pin being taken, or a pin being cleared) rather than a
free rewrite — a wrong write is repairable, never silently doubled. On reconnect the old node
sees its object is no longer its own and tears the pod down; whatever was typed during the
partition and never replicated is gone. There is no lease a pod must renew to prevent this — that
would be a second liveness system on top of the Node object's own.

### ZeroFS failover (manual, by design)

ZeroFS runs `replicas: 1` with `strategy: Recreate` because it has one SlateDB behind it and
SlateDB has one writer — a rolling update that let a second pod start before the first stopped
would be exactly the fenced-handle bug the ownership map exists to prevent elsewhere. It is also
pinned to the `k3s-cp` control-plane node by `nodeSelector`/toleration. Put together: if that
node is lost, every home in the region is unavailable until it comes back — there is no
automatic failover for this pod.

Recovery is one of:

- Bring `k3s-cp` back. The pod reschedules there and homes resume.
- Move ZeroFS to another node: edit its `nodeSelector`/toleration in `zerofs.yaml` and apply —
  the moved pod still needs the `zerofs-store` Secret to exist in `rustic-git-system` on the new
  node's namespace scope (it already does; Secrets aren't node-scoped, this is just a reminder
  the pod won't come up without it). **Before applying, confirm the old pod is actually gone** —
  `kubectl -n rustic-git-system get pod -l app=zerofs` must return empty. If it's stuck
  `Terminating` instead, first confirm the node is genuinely down —
  `kubectl get node k3s-cp` — and only if it shows `NotReady`/unreachable, force-delete the
  stuck pod: `kubectl -n rustic-git-system delete pod -l app=zerofs --force --grace-period=0`.
  That force-delete is the fencing decision: it tells Kubernetes to forget a pod it can no
  longer confirm is dead, so it must never be done to a node that is merely unreachable from
  here but still running — that node's kubelet could still have the old pod's SlateDB handle
  open, and starting a second pod against the same prefix is exactly the fencing this repo's
  one-writer rule forbids.

Either way, once the (new) ZeroFS pod is `Ready`, restart the agents so their NFS mounts
re-resolve to it: `kubectl -n kube-system rollout restart ds/rustic-git-agent`. Skipping this
leaves agents on stale NFS file handles, which answer `EIO` rather than re-mounting on their own.

## Rollout: 2026-09-02 hardening

The order below is not a preference. Steps 5 and 6 both touch the NFS home path, step 8 depends
on 6, and step 9 can lock you out of a node; everything else is independent. Do them one at a
time, on a normal weekday, with `KUBECONFIG` pointed at the prod cluster (see "Iterating"
above for where that file lives) and `cd deploy/k3s`. Every step below names its own
check and its own rollback — if a check fails, roll that step back before starting the next one.

1. **NSG (`provision-azure.sh`).** From `deploy/k3s`, with `env.sh` filled in, run
   `API_CLIENTS=<the api tier's egress CIDRs, comma-separated> ./provision-azure.sh`. It is
   idempotent: every rule is guarded by a `show`, and the VMs already exist so nothing is
   created. *Check:* `az network nsg rule list -g "$RG" --nsg-name k3s-nsg -o table` shows
   exactly ONE Allow rule on port 80 — `gateway-cloudflare` at priority 120. If a second Allow-80
   rule appears (an older `allow-http-cloudflare` at 230), delete it:
   `az network nsg rule delete -g "$RG" --nsg-name k3s-nsg -n allow-http-cloudflare`.
   *Rollback:* `az network nsg rule delete` the rule you did not want; the NSG is the only state
   this step writes.

2. **`workspace-admission.yaml`.** Pre-checks first, all three must pass:
   `kubectl get runtimeclass gvisor` exists;
   `kubectl -n kube-system get secret rustic-git-agent -o jsonpath='{.data.WS_RUNTIME_CLASS}' | base64 -d`
   prints `gvisor`; and every workspace pod already runs under it —
   `kubectl get pod -A -l rustic-git.io/kind -o custom-columns=NS:.metadata.namespace,NAME:.metadata.name,RC:.spec.runtimeClassName`
   shows `gvisor` on every row, no `<none>`. Only then
   `kubectl apply -f workspace-admission.yaml`. *Check:* create one workspace (`kl ws create …`)
   and watch it reach `Running` — the policy refuses pods at admission, so a bad policy shows up
   as a workspace stuck in `Creating` with a denial event on its namespace.
   *Rollback:* `kubectl delete validatingadmissionpolicybinding rustic-git-workspace-pod-fence`
   (delete the binding, not the policy — a policy with no binding enforces nothing).

3. **`agent-rbac.yaml` + `agent-admission.yaml`, together.** They are one change: the role is
   only true because the policy enforces it. `kubectl apply -f agent-rbac.yaml -f agent-admission.yaml`.
   *Check:* over a FULL reconcile pass — wait five minutes, long enough for every beat including
   the sync beat (`WS_SYNC_SECS`, default 60s) to have run —
   `kubectl -n kube-system logs ds/rustic-git-agent --since=5m | grep -i forbidden` stays empty.
   A single `forbidden` line means the role lost a verb the controller uses — do not leave it.
   *Rollback:* delete the two bindings
   (`kubectl delete validatingadmissionpolicybinding rustic-git-agent-spec-is-read-only rustic-git-agent-tenant-namespaces-only`)
   and re-apply the previous revision of both files from git — deleting the ClusterRoleBinding
   instead would stop the agent dead, which is not a rollback.

4. **`api-rbac.yaml`.** `kubectl apply -f api-rbac.yaml`. *Check:* a **new** owner — one who has
   never had a workspace, so their namespace and `user-key` Secret do not exist yet — creates
   their first workspace and can `kl ws ssh` into it. That is the only path that exercises
   `secrets: create`; an existing owner's key is an update and would pass either way.
   *Rollback:* re-apply the previous revision of the file from git.

5. **`zerofs.yaml` — maintenance window.** `strategy: Recreate` with one replica means the export
   goes DOWN for the restart, and every home with it; `hard` mounts block rather than fail, so
   running workspaces hang until it returns. Announce it. Before applying, confirm the image
   digest in the file really is ZeroFS 2.3.2 (`grep image: zerofs.yaml`, then
   `docker buildx imagetools inspect <ref>` or the upstream release page) — a digest bump is the
   whole change and pinning the wrong one is a silent downgrade. Apply, wait for
   `kubectl -n rustic-git-system rollout status deploy/zerofs`, then IMMEDIATELY
   `kubectl -n kube-system rollout restart ds/rustic-git-agent` — agents hold stale NFS file
   handles across a ZeroFS restart and answer `EIO` instead of remounting.
   *Rollback:* re-apply the previous revision (the old digest) and restart the agents again.

6. **`system-netpol.yaml` — NOT in the same minute as step 5.** Give step 5's agents time to
   remount and prove healthy first, or a hung home is ambiguous between the two changes.
   `kubectl apply -f system-netpol.yaml`. *Check:* restart ONE agent
   (`kubectl -n kube-system delete pod <agent pod>`), wait for it to be `2/2 Running`, then
   `kubectl -n kube-system exec <that pod> -c agent -- ls /wspool-prod/homes` — it must list the
   owners' directories, promptly. Pick an agent on a node OTHER than `k3s-cp`: the ZeroFS node's
   own mount is admitted by the node-subnet rule and would pass even if every `flannel.1` /32 were
   wrong. A hang here means the policy is not matching that node's masqueraded source address —
   check it against `ip -4 -o addr show flannel.1` on that node. *Rollback:* `kubectl -n rustic-git-system delete networkpolicy zerofs-nfs-from-agents`
   **and then restart that agent pod** — a `hard` NFS mount that is already wedged does not
   recover when the policy is removed; only a fresh mount does.

7. **`agent-peer.yaml`.** `kubectl apply -f agent-peer.yaml`, any time — it only narrows who may
   reach 8444/9464. *Check:* a `kl ws push` on a workspace with replicas still reports its
   replica `Synced`. *Rollback:* re-apply the previous revision.

8. **`agent-daemonset.yaml` — only after step 6 is verified good.** It adds a nix-daemon sidecar
   requesting 500m CPU / 1Gi memory on every pooled node, so check there is room first:
   `kubectl describe node | grep -A6 "Allocated resources"` on each node must leave that much
   unrequested, or the DaemonSet pod sits `Pending` and that node serves no workspaces.
   `kubectl apply -f agent-daemonset.yaml`, then
   `kubectl -n kube-system rollout status ds/rustic-git-agent`.
   *Rollback:* `kubectl -n kube-system rollout undo ds/rustic-git-agent`.

9. **`harden-node.sh` — one node at a time, last.** This one can lock you out: it drops by default
   on the public NIC. Before touching a node, open a SECOND ssh session to it and LEAVE IT OPEN
   (that session is the rollback channel), and have the Azure serial console open in a browser as
   the backstop. Then, per node:
   `CF_CIDRS=$(paste -sd, cloudflare-ips-v4.txt) ssh <node> sudo CF_CIDRS="$CF_CIDRS" bash -s < harden-node.sh`.
   *Check, all four, before moving to the next node:* `kubectl get node <n>` still `Ready`; that
   node's agent log has no apiserver connection errors
   (`kubectl -n kube-system logs <that node's agent pod> --since=2m`); `kl ws ssh` into a
   workspace running on that node succeeds; and a `kl ws push` from it completes.
   *Rollback, from the held session:* `nft delete table inet node` — the whole ruleset is one
   table, so that one command restores the previous (open) state instantly.

The Rust-side changes in this batch — the narrowed `allow-dns` egress rule and the `kl` host-key
pin — are NOT in any step above. They ship as code: merge to master, wait for the image build,
`deploy/pin.sh <sha>`, commit, `deploy/roll.sh`. Applying a manifest cannot deliver them.
