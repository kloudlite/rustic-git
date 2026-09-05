# The k3s workload cluster

Workspaces and environments run here. The git/registry server tier does not — it stays on its own
cluster and this one reaches it over HTTP.

Files, in the order a cluster is built:

| File | What it does |
| --- | --- |
| `env.example.sh` | Copy to `env.sh` (git-ignored) and edit. Sizes, names, operator CIDR, `API_CLIENTS` (the api tier's egress, admitted to 6443 by both the NSG and nftables), `CF_IPS_FILE` (defaults to `cloudflare-ips-v4.txt`). |
| `provision-azure.sh` | VNet, NSG (including the Cloudflare-on-80 and api-tier-on-6443 rules, from `CF_IPS_FILE`/`API_CLIENTS`), control plane, workers, build VM. Idempotent — re-run after a partial failure. |
| `format-pool.sh` | Run on each worker with the data disk as argument. btrfs at `/wspool-prod`. |
| `crds.yaml` | **Generated** — do not hand-edit. `CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml`. Now also carries `Region`: apply this and `api-rbac.yaml` before rolling the api image, then register each region with `POST /v1/regions` rather than seeding it into Cosmos. |
| `agent-rbac.yaml` | ServiceAccount + ClusterRole for the node controller. The header table is the role: one row per call the agent makes. |
| `agent-admission.yaml` | The ValidatingAdmissionPolicy that makes the role true — refuses the agent any spec write but `Volume.spec.restoreTo`, and pins every namespaced object it writes — pods, statefulsets, services, networkpolicies, limitranges, Secrets, RoleBindings — to the `ws-`/`wt-`/`env-` namespaces it makes. Apply with `agent-rbac.yaml`, always. |
| `workspace-admission.yaml` | The ValidatingAdmissionPolicy that puts PSA `baseline`'s refusals back for workspace/environment pods (`hostNetwork`/`hostPID`/`hostIPC`, privileged containers, stray `hostPath` sources) now that the namespace floor is `privileged`. Matches on namespace, not identity — safe to apply any time, even before an agent rollout. |
| `agent-daemonset.yaml` | The node controller itself, one pod per pooled node. |
| `agent-peer.yaml` | NetworkPolicy admitting the replication listener (port 8444) only from other agent pods, and metrics (9464) only from the OTel collector's pod in `kube-system` (`otel-agent.yaml`). It used to name a namespace called `monitoring` that never existed, so 9464 was unreachable and the agent's gauges went nowhere. No Service — discovery is by pod IP from the API. See "Replication" below. |
| `otel-agent.yaml` | The region's OpenTelemetry collectors: ServiceAccount + ClusterRole (the header table is the role), a DaemonSet for the per-node receivers (kubelet stats, pod logs, and this node's `prometheus.io/scrape` pods) and a one-replica Deployment for `k8s_cluster`. Exports to the ClickStack gateway on AKS. Needs the `kloudlite-git-otel` Secret first (`../clickstack/README.md`) and `KLOUDLITE_GIT_REGION` edited to this region's id. |
| `harden-node.sh` | Node firewall (drop-by-default on the public NIC), unattended upgrades, keys-only sshd. Idempotent; run on every node after provisioning and after changing the operator CIDR, and again with `CF_CIDRS` set once the gateway is live. Streamed over `ssh … sudo bash -s < harden-node.sh`, so `CF_CIDRS` must be passed as an env var on the remote command, not read from a local file — see the Gateway section below. |
| `cloudflare-ips-v4.txt` | Cloudflare's published v4 edge ranges, one CIDR per line — the one source. Build `CF_CIDRS` from it locally (`paste -sd, cloudflare-ips-v4.txt`) before running `harden-node.sh`. Refreshed by `../cf-sync.sh`, which also renders the AKS-side copies (`../ingress-nginx-service.yaml`, `../ingress-nginx-config.yaml`) and is run weekly by CI; never edit by hand. A stale list fails safe (the new edge is just refused, never wrongly trusted). |
| `gateway.yaml` | The workspace SSH gateway: one pod per pool node on the node's own `hostPort: 80`, behind the Cloudflare proxy (TLS ends at the edge). In its own `kloudlite-git-system` namespace, which the workspace NetworkPolicy names (`k8s::GATEWAY_NAMESPACE`). |
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
kubectl apply -f crds.yaml -f agent-rbac.yaml -f agent-admission.yaml -f workspace-admission.yaml -f nix-conf.yaml -f agent-daemonset.yaml -f agent-peer.yaml -f gateway.yaml -f system-netpol.yaml -f otel-agent.yaml -f quotas-slo.yaml
```

### The SLO probe's two owners

`quotas-slo.yaml` is above because the probe (`deploy/kloudlite-git.yaml`'s three CronJobs) runs
as `slo-probe` and `slo-other` in THIS cluster, and without the two `Quota` objects it inherits
`default-user` — a real person's allowance, which makes `quota.refused` cost a person's worth of
workspaces before it sees its 409. Safe to re-apply; nothing else references them.

```sh
kubectl apply -f quotas-slo.yaml

# The probe's SSH key, once, on the AKS side (it is a Secret there, not here). Generate it and
# register the public half; the runs use the private one for every SSH step.
ssh-keygen -t ed25519 -N '' -C slo-probe -f /tmp/slo_ed25519
kubectl -n kloudlite-git create secret generic kloudlite-git-slo --from-file=ssh_key=/tmp/slo_ed25519
shred -u /tmp/slo_ed25519            # the cluster has it now; a copy on a laptop is a second key to lose

# The probe's own identity on this region: apply slo-rbac.yaml (the header table is the role —
# exec into its pods, cordon/taint for the drills, impersonate the agent for one dry-run), then
# mint a kubeconfig for the `kloudlite-git-slo` ServiceAccount exactly as "Rotating the api
# tier's kubeconfig" below does for the api's, and store it on AKS as
# kloudlite-git-slo-k3s-kubeconfig. Without it stages 5-7 cannot see the workspace CRDs.
kubectl apply -f slo-rbac.yaml

# Then claim the two usernames, once. Idempotent.
# `create job --from` copies the pod template verbatim and takes no argument override, so the
# subcommand is swapped in on the way past.
kubectl -n kloudlite-git create job slo-bootstrap --from=cronjob/kloudlite-git-slo-fast \
  --dry-run=client -o json | jq '.spec.template.spec.containers[0].args=["bootstrap"]' | kubectl apply -f -
```

Every object a run creates is named `run-{run_id}` and swept by name prefix, so leftovers from a
crashed run are cleaned up by the NEXT run rather than accumulating. To see them:
`kubectl get workspaces,environments,snapshots -l kloudlite-git.io/owner=slo-probe`. Deleting a
`run-*` object by hand is always safe — nothing outside the probe references one — and is the
right move only if the probe itself is stopped and the objects are holding quota.

### Upgrading an existing cluster off PersistentVolumes

`agent-rbac.yaml`'s ClusterRole no longer grants the agent `get`/`list`/etc. on `PersistentVolume`
— the hostPath rework deleted that need. Applying the new RBAC and the new DaemonSet together is
**unsafe**: the ClusterRole is cluster-scoped and takes effect the instant it's applied, but a
DaemonSet rollout is not instant. An old agent pod still running on a not-yet-rolled node calls
`get` on a PV as part of its reconcile, gets back 403 instead of the old behavior, and its **entire
reconcile pass aborts** — every workspace on that node stops converging until that node's pod is
replaced. Apply in this order:

1. Pin and apply `agent-daemonset.yaml` only, and **wait for the rollout to finish on every node**
   (`kubectl rollout status daemonset/kloudlite-git-agent -n <ns>`) before moving on.
2. Only then apply `agent-rbac.yaml` (and `agent-admission.yaml`).
   `workspace-admission.yaml` is the exception to this ordering: it is deny-only, matches on
   namespace rather than agent identity, and requires nothing from the new agent, so apply it
   whenever convenient — before step 1 is fine too.
3. Then run the cutover deletes below: pods, then pvc, then pv.

```sh
kubectl delete pods -A -l kloudlite-git.io/kind=workspace
kubectl delete pods -A -l kloudlite-git.io/kind=environment
kubectl delete pvc -A -l kloudlite-git.io/kind=volume
kubectl delete pv -l kloudlite-git.io/kind=volume
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
kubectl label node <node> kloudlite-git.io/pool=true          # has a btrfs pool: run the controller here
kubectl label node <node> kloudlite-git.io/session=true       # may host workspaces
kubectl label node <node> kloudlite-git.io/env=true           # may host environments
```

One key per role, not `role=session`, because a label key holds one value and a small cluster needs
one node to be both.

```sh
# N. The new node's flannel /32, BEFORE its agent first mounts. `system-netpol.yaml` allow-lists
#    NFS (2049) by node and flannel address; a missing entry is not an error anyone sees — the
#    mount times out and every workspace on that node parks in `HomeNotReady`.
ssh <node> ip -4 -o addr show flannel.1     # e.g. 10.42.6.0 — add `- ipBlock: {cidr: 10.42.6.0/32}`
kubectl apply -f system-netpol.yaml
```

Then start the agent on the new node (the DaemonSet schedules it once the labels above are set).

## Control-plane backup

The CRDs are one SQLite file on one VM. `backup-controlplane.sh` copies it (plus the cluster
identity and a `kubectl get … -o yaml` of every object) to the `k3s-backup` container every hour,
but only once the timer is installed — this is the step that was missing. Once, on `k3s-cp`:

```sh
# 1. A SAS on container k3s-backup with create+write only (no list/delete: the script rotates by
#    overwriting fixed names, so a leaked SAS cannot read or destroy history), and a healthchecks-style
#    monitor URL with a 1 h period. Both by hand on the node:
ssh azureuser@<k3s-cp> 'sudo install -d -m700 /etc/kloudlite-git \
  && sudo sh -c "umask 077; cat > /etc/kloudlite-git/k3s-backup.sas" \
  && sudo sh -c "echo SNITCH_URL=https://hc-ping.com/<uuid> > /etc/kloudlite-git/k3s-backup.env"'
# 1b. The encryption key. The bundle carries the cluster CA, the SA signing key and the join
#     token, so it is encrypted before it leaves the node and the blobs are useless without this
#     file. Generate it once, then PUT A COPY IN THE PASSWORD MANAGER: a restore onto a fresh node
#     starts from the vault copy, and a lost key makes every backup noise.
ssh azureuser@<k3s-cp> 'sudo sh -c "umask 077; openssl rand -hex 32 > /etc/kloudlite-git/k3s-backup.key"; sudo cat /etc/kloudlite-git/k3s-backup.key'
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
up. The account defaults to `kloudlitegitkolomi`; override `ACCOUNT`/`CONTAINER` in the `.env` file.
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
`kloudlite-git-k3s-kubeconfig` Secret, mounted at `/etc/kloudlite-git/k3s` on the `kloudlite-git-api`
Deployment (`deploy/kloudlite-git.yaml`) and pointed at by `KUBECONFIG` there. It is a stopgap — the
`ponytail:` note beside that env var says the pull design in
`docs/superpowers/specs/2026-08-26-cluster-sync-design.md` replaces it with each cluster syncing
its own desired state, at which point this Secret goes away entirely. Until then, rotate it:

```sh
# 1. A fresh bound token for the api's own ServiceAccount (deploy/k3s/api-rbac.yaml), scoped to
#    this cluster only — nothing wider than what /v1 already had.
KUBECONFIG=.local/k3s.yaml kubectl -n kube-system create token kloudlite-git-api --duration=8760h > /tmp/api.token

# 2. Build a kubeconfig around it with the cluster's CA (already on disk from provisioning, or
#    pull it fresh):
KUBECONFIG=.local/k3s.yaml kubectl config view --raw --minify -o jsonpath='{.clusters[0].cluster.certificate-authority-data}' \
  | base64 -d > /tmp/api-ca.crt
API_SERVER=$(KUBECONFIG=.local/k3s.yaml kubectl config view --raw --minify -o jsonpath='{.clusters[0].cluster.server}')
kubectl config set-cluster k3s --server="$API_SERVER" --certificate-authority=/tmp/api-ca.crt --embed-certs=true --kubeconfig=/tmp/api.kubeconfig
kubectl config set-credentials kloudlite-git-api --token="$(cat /tmp/api.token)" --kubeconfig=/tmp/api.kubeconfig
kubectl config set-context default --cluster=k3s --user=kloudlite-git-api --kubeconfig=/tmp/api.kubeconfig
kubectl config use-context default --kubeconfig=/tmp/api.kubeconfig

# 3. Replace the Secret in the AKS cluster (default context — this is a different cluster from
#    the k3s one above), and roll the api pods to pick it up:
kubectl create secret generic kloudlite-git-k3s-kubeconfig --from-file=config=/tmp/api.kubeconfig \
  --dry-run=client -o yaml | kubectl -n kloudlite-git apply -f -
kubectl -n kloudlite-git rollout restart deploy/kloudlite-git-api

# 4. Verify with a live read through the new token, then let the old token expire — a bound token
#    has no separate revoke call, so "the old token stops working" means deleting the Secret entry
#    is not enough; the old token is only dead once its --duration expires or the ServiceAccount
#    it was bound to is deleted and recreated. Rotating on the schedule below is what keeps that
#    window short.
curl -s https://api.kloudlite-git.example/v1/regions -H "Authorization: Bearer <api client token>" | head -c 200

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
lives on AKS in the `kloudlite-git` namespace, on the default context.

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
KUBECONFIG=.local/k3s.yaml kubectl rollout status ds/kloudlite-git-agent -n kube-system

# 5. Watch the startup migration adopt the existing objects. Every line it writes is prefixed
#    `migration:`; every workspace must end with a node in STATUS, not only in spec.
KUBECONFIG=.local/k3s.yaml kubectl logs -n kube-system -l app=kloudlite-git-agent --tail=200 \
  | grep migration:
KUBECONFIG=.local/k3s.yaml kubectl get workspaces \
  -o custom-columns=NAME:.metadata.name,SPEC:.spec.nodeName,STATUS:.status.nodeName,VOL:.status.volumeRef

# 6. Only then the API tier, on AKS (deploy/kloudlite-git.yaml, pinned to CI's SHA by deploy/pin.sh;
#    deploy/roll.sh applies it in one go).
deploy/roll.sh
kubectl rollout status deploy/kloudlite-git-api -n kloudlite-git
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
#    VolumeReplica's `.spec.volume` selector (added 2026-09-02): apply it BEFORE the agent image
#    that reads it — an agent ahead of the CRD gets a 400 on every stopped parent's `Replicated`
#    recompute, not on replication.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/crds.yaml

# 2. Roll the agent (repin the image tag to the SHA CI built, then apply and wait).
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-daemonset.yaml
KUBECONFIG=.local/k3s.yaml kubectl rollout status ds/kloudlite-git-agent -n kube-system

# 3. Then the API tier (kloudlite-git.yaml) — deploy/roll.sh applies it.
deploy/roll.sh
kubectl rollout status deploy/kloudlite-git-api -n kloudlite-git
```

Pre-existing asymmetry, follow-up only: `live_parents` in `crates/workspaces/src/api.rs` selects
Workspaces by a single owner (`owned_by`) but Environments by the caller's whole owner set
(`owner_set_selector`), so a team environment shows here while a lone workspace under a teammate's
name would not.

**Every existing volume's pod restarts once — and, for homes, force it explicitly.** There is no
bulk migration step: an old-layout volume (`{pool}/vol/{id}/live` is itself the RW subvolume) is
migrated the first time it's CLAIMED on a node under the flag (`migrate_and_seed_baseline` in
`bins/agent/src/controller/workspace.rs`, calling `Engine::migrate_volume`), which moves `live` to a
directory holding one worktree, `live/{id}` — and the pod that was mounting the OLD path has to be
recreated to pick up the new one, exactly like the hostpath cutover above. A workspace/environment
volume migrates lazily, the first time it's claimed, so its pod restarting on its own schedule is
enough. **A home does not wait for a claim: the home beat migrates every ready home on every pass
(H3), so its already-running pod's hostPath (`home_volume` in `crates/workspaces/src/k8s.rs`) goes
stale on the FIRST post-rollout beat** — `ensure`'s Server-Side Apply then 422s against that pod's
immutable `hostPath` until it's deleted. Same step as the hostpath cutover's Step 3, run right
after step 2 above:

    kubectl delete pods -A -l kloudlite-git.io/kind=workspace
    kubectl delete pods -A -l kloudlite-git.io/kind=environment

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

The `kloudlite-git-agent` Secret's `AZURE_*` keys become unused (the env wiring is already gone); the
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

## Shared homes: Azure Files Premium (NFS 4.1)

Every owner's `/home/kl` in a region is `{pool}/homes/{owner}` on ONE managed NFS 4.1 share —
Azure Files Premium, account `kloudlitegithomes` (Premium_ZRS, `rustic-git-k3s`), share `homes`,
reachable only from the `nodes` subnet through its `Microsoft.Storage` service endpoint. Nothing
of ours runs in the path: the agent mounts it at boot (`WS_HOMES_EXPORT` in
`agent-daemonset.yaml`, `vers=4,minorversion=1,sec=sys`), and the provider's SLA is the
availability story. ZeroFS (a single pod serving NFSv3 over a blob-backed LSM tree) was retired
on 2026-09-04 after its segment GC started leaking; this replaced it.

Per region: a new region gets its own account and share in its own VNet (homes are region-local
by design), the same three commands:

```sh
az network vnet subnet update --ids <nodes subnet id> --service-endpoints Microsoft.Storage
az storage account create -n <account> -g <rg> -l <location> --kind FileStorage --sku Premium_ZRS \
  --https-only false --default-action Deny
az storage account network-rule add -g <rg> -n <account> --subnet <nodes subnet id>
az storage share-rm create -g <rg> --storage-account <account> -n homes \
  --enabled-protocols NFS --root-squash NoRootSquash --quota 100
```

`--https-only false` is required (NFS is not TLS) and `--default-action Deny` plus the subnet rule
is what keeps the share off the internet. The quota is provisioned capacity (billed) — 100 GiB is
the minimum and holds years of dotfiles; raise it only when `df` on a node says so.

## Gateway

The workspace SSH gateway runs on the pool nodes themselves (`session-0`, `env-0`) behind
Cloudflare — no LoadBalancer, no tunnel connector. Operator steps, once per region:

1. DNS (Cloudflare dashboard): **A** records for `ws-<region>.khost.dev` → each pool node's
   public IP, both **proxied**.
2. SSL/TLS mode **Full (strict)** for the zone.
3. SSL/TLS → Origin Server → Create Certificate (15 years) for `ws-*.khost.dev`, then
   `kubectl -n kloudlite-git-system create secret tls gateway-tls --cert=<cert> --key=<key>`.
4. Copy the `kloudlite-git-jwt` Secret from AKS into this cluster's `kloudlite-git-system` (the gateway
   verifies session tokens locally, with the same secret the api mints them with), and check
   the `WS_REGION` in `gateway.yaml`'s ConfigMap equals the agent Secret's. Moving the gateway
   out of `kube-system` (2026-08-29) is a roll in this order: the agent first (its next reconcile
   rewrites every workspace's `allow-gateway-ssh` policy to the new namespace), then
   `kubectl apply -f gateway.yaml`, then `kubectl -n kube-system delete deploy,svc,sa
   kloudlite-git-gateway`. SSH sessions drop once, between the agent roll and the new gateway
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

**The agent Secret is not in this directory.** `kloudlite-git-agent` in `kube-system` carries
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
off first (`kubectl label node <node> kloudlite-git.io/pool-`) and put it back after.

## Replication

Volume standbys, pull-based: every node's agent decides which commits it must hold (replication's
rendezvous names it, or it runs one of the volume's worktrees) and GETs them from a peer that
already has them. Bring it up:

1. Add `WS_PEER_SECRET` to the `kloudlite-git-agent` Secret (same one as `WS_REGION` etc. above) —
   any shared string, compared in constant time on every peer request. Unset, the peer listener
   never starts and the puller never dials: fail-closed, not fail-open.
2. Apply `agent-peer.yaml` (already in the fresh-cluster command above) so the listener's port
   8444 is reachable only from other `app: kloudlite-git-agent` pods — without it, every pod in the
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
a leak. `WS_SYNC_SECS` (default 60) is how often the beat checks. A stop no longer waits for anything: the
cut turns Ready, the pod goes, and the owner pokes every placeable peer with `/peer/v1/wake` so the
copy happens in seconds. Whether it HAS happened is the `Replicated` condition on the stopped
object — `kubectl get workspace X -o jsonpath='{.status.conditions[?(@.type=="Replicated")]}'`.
`False/AwaitingReplica` with "no other node holds the final sync point yet" is normal for a few
seconds after a stop and is only worth investigating if it persists (check `WS_PEER_SECRET`, above
— replication is fail-closed without it); with the message "no replica is configured for this
volume" it will never become true (that is `spec.replicas: 1`), and the workspace simply always
starts on its own node.

### Node death

A node is dead once its `Node` object has been NotReady for `WS_NODE_DEAD_SECS` (default 180, enough for a reboot to finish without healing around it; the DaemonSet no longer overrides it) —
that floor, not a shorter probe, is what keeps a brief kubelet hiccup from tearing anything down.
Every surviving agent's pull beat then does two things to that node's volumes: replicas heal onto
a third live node automatically (rendezvous stops naming the dead node as a candidate, so a
survivor picks up the missing copy on its own), and the sweep decides what happens to each `Volume`
the dead node owned. The decision is PER VOLUME, not per workspace, because ownership is per volume
— three arms, in order:

1. any parent on it is Running → nothing moves. The volume goes `status.phase: Unavailable` with
   `Available=False/NodeDead` and every parent gets `Degraded=True/NodeDead`, because that
   worktree's edits since the last sync point exist only on that node's disk.
2. otherwise, any stopped parent whose `Replicated` condition is not yet `True` → nothing moves
   either, and the condition's message names which parents are holding it. Starting elsewhere
   before its bytes are anywhere else would lose them.
3. otherwise → the owner pin is cleared and every parent is un-placed, so the next start lands on
   whichever node is up to date for that worktree.

`kubectl get volumes` is the fast way to see which ones are affected, and the `Replicated`
condition on a stopped parent is the fast way to see why one is stuck on arm 2.

A running worktree on a dead node is INTERRUPTED, not moved: `/v1` refuses to start it with a 409
("its node is down; it resumes when the node returns"), and the way forward is either waiting for
the node or cloning it — a workspace clone grafts onto the newest sync point some live node holds
and says so in the response's `based_on` (snapshot, time, age, `interrupted: true`), so choosing to
lose the last few seconds is always a person's choice. An interrupted ENVIRONMENT cannot be cloned
at all (409): an environment clone copies bytes from the source's own live subvolume, and there is
no live node holding it. Stopping an interrupted parent is always accepted — it is a spec write —
and its own controller performs the stop when the node returns. If the node comes back before
anyone stops it, its pin was never cleared, so nothing moves: the pod resumes with everything
intact.

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

### Decommissioning a node

The planned version of node death. It never stops anyone's work; the node drains at the people's
pace, and an operator in a hurry stops those workspaces through `/v1` like anyone else.

1. `kubectl label node <n> kloudlite-git.io/decommission=true`. From that moment every other agent
   treats it as unplaceable — it wins no rendezvous slot, counts as no copy, and refuses claims —
   while it keeps serving pulls and keeps running everything already on it. Running parents get
   `Decommissioning=True/NodeLeaving` ("this node is being retired; stop when convenient and the
   next start lands elsewhere") so the person is told, and nothing is marked `Unavailable`: the
   node is alive and the work on it is healthy.
2. Watch the one annotation: `kubectl describe node <n> | grep decommission-status`, or
   `kubectl get node <n> -o jsonpath='{.metadata.annotations.kloudlite-git\.io/decommission-status}'`.
   It reads `draining running=N owned=N copies=N thin=N` and is rewritten every
   `WS_DECOMMISSION_SECS` (default 30). `running` is people's workspaces — it only falls when they
   stop them. `owned` falls as each volume becomes releasable (everything on it stopped AND
   replicated); `copies` falls as its replicas re-home and its own retire pass drops them. `thin`
   is durability, not residency: volumes whose bytes are still on this node and which OTHER nodes
   do not yet hold `spec.replicas - 1` Synced copies of. It is what stops the gate opening on a
   volume that is one node away from having no redundancy left.
3. When all four reach zero the annotation becomes `drained <RFC 3339>`, and it is sticky: it
   records when the node drained, not when we last looked.
4. Only then delete the VM, and remove that node's flannel `/32` from the `ipBlock` list in
   `deploy/k3s/system-netpol.yaml` (read one off a node with `ip -4 addr show flannel.1`; the list
   is hand-maintained, see the comment on that file's `ipBlock`).

Deleting the VM before `drained` is the dead-node path: copies still heal, but any volume not yet
released waits for a node that will never return. That is the whole reason `drained` is a gate and
not just a progress line.

To abort, remove the label: the beat stops immediately, parents already stopped stay stopped (start
them — they run here again if the volume was not released, elsewhere if it was), copies already
re-homed stay re-homed, and the node becomes a rendezvous candidate again.

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
   `kubectl -n kube-system get secret kloudlite-git-agent -o jsonpath='{.data.WS_RUNTIME_CLASS}' | base64 -d`
   prints `gvisor`; and every workspace pod already runs under it —
   `kubectl get pod -A -l kloudlite-git.io/kind -o custom-columns=NS:.metadata.namespace,NAME:.metadata.name,RC:.spec.runtimeClassName`
   shows `gvisor` on every row, no `<none>`. Only then
   `kubectl apply -f workspace-admission.yaml`. *Check:* create one workspace (`kl ws create …`)
   and watch it reach `Running` — the policy refuses pods at admission, so a bad policy shows up
   as a workspace stuck in `Creating` with a denial event on its namespace.
   *Rollback:* `kubectl delete validatingadmissionpolicybinding kloudlite-git-workspace-pod-fence`
   (delete the binding, not the policy — a policy with no binding enforces nothing).

3. **`agent-rbac.yaml` + `agent-admission.yaml`, together.** They are one change: the role is
   only true because the policy enforces it. `kubectl apply -f agent-rbac.yaml -f agent-admission.yaml`.
   *Check:* over a FULL reconcile pass — wait five minutes, long enough for every beat including
   the sync beat (`WS_SYNC_SECS`, default 60s) to have run —
   `kubectl -n kube-system logs ds/kloudlite-git-agent --since=5m | grep -i forbidden` stays empty.
   A single `forbidden` line means the role lost a verb the controller uses — do not leave it.
   *Rollback:* delete the two bindings
   (`kubectl delete validatingadmissionpolicybinding kloudlite-git-agent-spec-is-read-only kloudlite-git-agent-tenant-namespaces-only`)
   and re-apply the previous revision of both files from git — deleting the ClusterRoleBinding
   instead would stop the agent dead, which is not a rollback.

4. **`api-rbac.yaml`.** `kubectl apply -f api-rbac.yaml`. *Check:* a **new** owner — one who has
   never had a workspace, so their namespace and `user-key` Secret do not exist yet — creates
   their first workspace and can `kl ws ssh` into it. That is the only path that exercises
   `secrets: create`; an existing owner's key is an update and would pass either way.
   *Rollback:* re-apply the previous revision of the file from git.



7. **`agent-peer.yaml`.** `kubectl apply -f agent-peer.yaml`, any time — it only narrows who may
   reach 8444/9464. *Check:* a `kl ws push` on a workspace with replicas still reports its
   replica `Synced`. *Rollback:* re-apply the previous revision.

8. **`agent-daemonset.yaml` — only after step 6 is verified good.** It adds a nix-daemon sidecar
   requesting 500m CPU / 1Gi memory on every pooled node, so check there is room first:
   `kubectl describe node | grep -A6 "Allocated resources"` on each node must leave that much
   unrequested, or the DaemonSet pod sits `Pending` and that node serves no workspaces.
   `kubectl apply -f agent-daemonset.yaml`, then
   `kubectl -n kube-system rollout status ds/kloudlite-git-agent`.
   *Rollback:* `kubectl -n kube-system rollout undo ds/kloudlite-git-agent`.

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

## Release: stop/decommission dead-field cleanup

The 2026-09-03 change drops four CRD fields that Tasks 1–11 of the stop/interrupt/decommission
work left with zero readers: `WorkspaceStatus.compatibleNodes` and `EnvironmentStatus.compatibleNodes`
(placement already reads the replica rows' `branches` instead), `WorkspaceStatus.durable` and
`EnvironmentStatus.durable`, `VolumeReplicaStatus.lastSyncAt`, and `OwnerBinding.spec.nodeName`.

**CRDs FIRST, then the agent** — the same order as "Order" above, and here it is load-bearing for
exactly one of the fields. `OwnerBinding.spec.nodeName` was REQUIRED in the old schema and the new
agent never sets it, so an agent rolled ahead of the CRDs gets a 422 on every `ensure_binding`
create and every workspace on that node parks in `NamespaceNotReady`. The other three
(`compatibleNodes`, `durable`, `lastSyncAt`) are genuinely order-free: they were optional, an old
agent that still writes one just has it pruned by the new schema, and nothing reads any of them.

## Release: snapshot state

The 2026-09-03 change adds `Snapshot.spec.state` (`crd::SnapshotState`), which every new cut
records and `restore` reads back as its default. The field is optional and additive, so this one
is order-free relative to the agent roll either way: applying the CRD first just means the running
agent doesn't populate it yet, and rolling the agent first means it writes a field the old CRD
schema doesn't know about (preserve-unknown-fields keeps it, not an error). Snapshots cut before
this change simply have no `spec.state`, and a restore from one falls back to the live source (or
the standard defaults, if that's gone too) exactly as it did before this release.

## Release: durable snapshots

The 2026-09-03 change makes a push outlive its workspace: a `Snapshot` a push writes is owned by
the `Volume`, the working copy's own cuts (sync points, `spec.transient: true`) are owned by the
workspace/environment, and every Workspace/Environment carries `WORKTREE_FINALIZER` so a delete
drops the worktree and the sync points and DETACHES the Volume when a snapshot remains
(`docs/superpowers/specs/2026-09-03-durable-snapshots-design.md` is the design).

**Apply the manifests BEFORE rolling the agent and the api**, all four, in this order:

```sh
# The api SA gains `snapshots: delete` and `volumes: delete` (api-rbac.yaml) — `/v1`'s deletes are
# CRD-backed now, so an api rolled ahead of its role 403s on every snapshot delete. The agent SA
# gains `volumes: delete` (agent-rbac.yaml) for `collect_unreferenced_volumes` — an ownerless
# volume with no worktree and no snapshot has no ownerReference left to garbage-collect it, and an
# agent rolled ahead of its role 403s on every retire beat. The agent's
# ownerReference patch needs no policy change (admission constrains spec only), and crds.yaml carries the reworded
# descriptions and drops the dead `pinned` property.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/crds.yaml \
  -f deploy/k3s/agent-rbac.yaml -f deploy/k3s/agent-admission.yaml -f deploy/k3s/api-rbac.yaml

# Then the agent, then the api tier.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-daemonset.yaml
KUBECONFIG=.local/k3s.yaml kubectl rollout status ds/kloudlite-git-agent -n kube-system
deploy/roll.sh
kubectl rollout status deploy/kloudlite-git-api -n kloudlite-git
```

No migration script, and nothing to run afterwards:

- **Every push already stored becomes a snapshot.** They are `spec.transient: false` already, which
  is exactly what a snapshot is now — intended, not an accident of the rollout.
- **Old migration baselines are recognised by SHAPE**, not by a flag: `transient: false`,
  `parent: ""`, message `"migration baseline"` reads as a sync point everywhere
  (`crd::Snapshot::is_snapshot`), so it stays out of history and never keeps a volume alive.
- **One extra sync cut per worktree on the rollout.** The beat now cuts when the derived definition
  differs from the newest sync point's as well as when the bytes moved, and the first pass of a new
  agent has no definition on record for the cut it inherits — so each running worktree takes one
  extra cut within a beat, then settles.
- `spec.pinned` and `WS_SNAPSHOT_KEEP` are gone. Stored `pinned` values are ignored by serde and
  pruned by the regenerated schema; retention now prunes sync points only, and a push is never
  pruned by anything.

## Release: quotas and the admin server

Two new CRDs (`Quota`, `QuotaRequest`) and a second `bins/api` process — the same binary started
with `KLOUDLITE_GIT_API_ROLE=admin` instead of the default `user` — that mounts `api::admin::router()`
and serves the superadmin-only surfaces (region creation, quota decisions,
`/api/admin/superadmins`) from its own host. Apply in this order:

```sh
# 1. The CRDs — adds `quotas` and `quotarequests`. Additive; existing objects are untouched.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/crds.yaml

# 2. api-rbac.yaml now defines TWO roles: the existing kloudlite-git-api SA keeps its read-only quota
#    surfaces, and a new kloudlite-git-admin SA/ClusterRole gets the only write access to
#    Quota/QuotaRequest/Region. Apply both here — the admin kubeconfig minted next needs the SA to
#    already exist.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/api-rbac.yaml

# 3. Mint the kloudlite-git-admin-k3s-kubeconfig Secret for the kloudlite-git-admin SA, the same recipe
#    as "Rotating the api tier's kubeconfig" above, adjusted to the admin SA and Secret name:
KUBECONFIG=.local/k3s.yaml kubectl -n kube-system create token kloudlite-git-admin --duration=8760h > /tmp/admin.token
KUBECONFIG=.local/k3s.yaml kubectl config view --raw --minify -o jsonpath='{.clusters[0].cluster.certificate-authority-data}' \
  | base64 -d > /tmp/admin-ca.crt
API_SERVER=$(KUBECONFIG=.local/k3s.yaml kubectl config view --raw --minify -o jsonpath='{.clusters[0].cluster.server}')
kubectl config set-cluster k3s --server="$API_SERVER" --certificate-authority=/tmp/admin-ca.crt --embed-certs=true --kubeconfig=/tmp/admin.kubeconfig
kubectl config set-credentials kloudlite-git-admin --token="$(cat /tmp/admin.token)" --kubeconfig=/tmp/admin.kubeconfig
kubectl config set-context default --cluster=k3s --user=kloudlite-git-admin --kubeconfig=/tmp/admin.kubeconfig
kubectl config use-context default --kubeconfig=/tmp/admin.kubeconfig
kubectl create secret generic kloudlite-git-admin-k3s-kubeconfig --from-file=config=/tmp/admin.kubeconfig \
  --dry-run=client -o yaml | kubectl -n kloudlite-git apply -f -
rm -f /tmp/admin.token /tmp/admin-ca.crt /tmp/admin.kubeconfig

# 4. The agent's role — no spec write changed (the agent creates/patches its namespace's
#    ResourceQuota, an ordinary namespaced object, not a Quota CR), but re-apply for the reworded
#    descriptions in the regenerated CRD.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-rbac.yaml
```

Then the AKS roll: `deploy/kloudlite-git.yaml` adds the `kloudlite-git-admin` Deployment and Service —
no Ingress and no DNS: the admin api is reached only server-side by the web through
`KLOUDLITE_GIT_ADMIN_API_URL=http://kloudlite-git-admin`, and the superadmin pages live on the app host
at `/superadmin`. Repin and roll it with the rest of the tier (`deploy/pin.sh`, `deploy/roll.sh`),
then `deploy/kloudlite-git-web.yaml` for that env var.

Set `KLOUDLITE_GIT_WORKSPACES_ADMINS` on the admin-role Deployment before its first boot: it seeds
those addresses into the directory's `superadmins` collection once, additively — after that boot
the list is managed only through `/api/admin/superadmins`, and removing an address from the env
revokes nobody.

**What existing owners see:** nothing changes until they cross a limit — `default-user`/
`default-team` apply to every owner with no `Quota` object of their own, and those numbers match
the compiled-in table, so a missing object was never a wider allowance. A quota blocks new
allocation only; nothing already running is touched.

## Release: live settings

One new CRD (`ClusterSettings`, singleton `default` per region) and no new process — the admin
`bins/api` gains settings routes on its existing router, and `kloudlite-git-agent`/the central tier
gain a refresh beat each. Apply in this order:

```sh
# 1. The CRD — adds `clustersettings`. Additive; nothing existing reads or writes it yet.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/crds.yaml

# 2. api-rbac.yaml: the kloudlite-git-admin ClusterRole gains create/patch on ClusterSettings and
#    patch (restart-annotation only, via the admission policy) on the KNOWN central workloads.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/api-rbac.yaml

# 3. agent-rbac.yaml: the agent ClusterRole gains get/list/watch on ClusterSettings — read-only,
#    same reasoning as its Quota row.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-rbac.yaml
```

Then the AKS roll: `deploy/kloudlite-git.yaml` adds the `kloudlite-git-admin` Role/RoleBinding for
patching the KNOWN_CENTRAL workloads' pod templates, and turns on
`automountServiceAccountToken: true` on the `kloudlite-git-admin` Deployment specifically (every
other Deployment here keeps it `false` — this is the one process whose own in-cluster token is
what lets it roll its siblings, distinct from the k3s kubeconfig Secret it already mounts for
`ClusterSettings`/agent workload rolls). Repin and roll with `deploy/pin.sh`/`deploy/roll.sh`,
then `deploy/kloudlite-git-web.yaml` for the settings admin UI.

**What existing readers see:** nothing, until a stored `cluster/settings` document or a
`ClusterSettings/default` CR is actually written — every field's `stored ??` branch is empty at
first boot, so every process runs exactly as it did on env alone.

## Release: superadmin console

RBAC only on this side — no new CRD, no new process.

```sh
# api-rbac.yaml: the kloudlite-git-admin ClusterRole gains `patch` on nodes, for the Clusters area's
# drain / undrain / decommission (the decommission label, the status annotation undrain clears,
# and spec.unschedulable). Nodes cannot be name-restricted, so the scoping is in the handler:
# one named node, in a region this cluster actually answers for, with a reason on the audit row.
# It also gains `get`/`list` on volumes, snapshots and volumereplicas: Owners, Clusters and
# Overview compute every number from those on the request path, so without them those three areas
# answer 403 outright rather than showing a stale count.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/api-rbac.yaml
```

Apply it BEFORE rolling the api image: without it a drain answers 403 from the k3s API server.

On the AKS side, `deploy/kloudlite-git.yaml`'s `kloudlite-git-admin-workloads` Role gains `get`/`list`
on `pods` (namespaced to `kloudlite-git`) — the Monitoring page's Signals scrape reads each pod's
`/metrics` and `restartCount` this way instead of assuming a Prometheus. Re-apply it before
rolling the admin image: without it Signals answers 403.

## Release: generic requests (2026-09-04)

`Request` generalizes `QuotaRequest` to quota/access/region/other kinds — new CRD, RBAC on it,
no new process.

```sh
# crds.yaml: adds Request.
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/crds.yaml

# api-rbac.yaml: adds `requests` to both ClusterRoles (kloudlite-git-api: get/list/create;
# kloudlite-git-admin: get/list/create/patch/delete plus patch/update on requests/status).
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/api-rbac.yaml
```

Apply both BEFORE rolling the api image: without the CRD every `/v1/requests` create 404s from
the API server, and without the RBAC the admin process 403s on the queue. Then, once per
cluster, `POST /admin/requests/migrate` with a note — idempotent, safe to repeat.

## Release: the history layer on ClickStack

ClickStack goes up FIRST, on AKS (`deploy/clickstack/README.md`), because the ingestion API key it
mints is what every region's collector needs; a collector applied before the Secret exists
CrashLoopBackOffs on a missing env var rather than starting and silently sending nothing. That
failure mode is chosen on purpose — see the `OTEL_INGESTION_KEY` comment in `otel-agent.yaml`.

```sh
# 1. AKS: the charts, then the API key (see deploy/clickstack/README.md), then the Secret in
#    every cluster.
# 2. Per region: the collector's Secret, then the collector, then the widened metrics policy and
#    the api RBAC the history layer's watches need.
KUBECONFIG=.local/k3s.yaml kubectl -n kube-system create secret generic kloudlite-git-otel \
  --from-literal=key='<ingestion key>'
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/otel-agent.yaml
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-peer.yaml
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/api-rbac.yaml
# 3. AKS: the admin Deployment's new env, after the ClickHouse user exists — the process logs
#    `history.migrations.applied` with `count` > 0 once, then `count=0` on every restart.
kubectl -n kloudlite-git apply -f deploy/kloudlite-git.yaml
```

`agent-peer.yaml`'s metrics rule previously admitted a namespace named `monitoring` that never
existed, so the agent's 9464 was unreachable; applying the new one is what makes the node gauges
arrive at all.

`api-rbac.yaml` adds `watch` to every read the `kloudlite-git-admin` ClusterRole already had. The
admin process runs one reflector per kind per region and turns the transitions into
`kloudlite.events`; without `watch` each one retries forever behind a backoff and the console's
charts stay empty while every other admin surface works. It grants no new authority — a `watch` is
a streamed `list`.

`deploy/kloudlite-git.yaml` gains the AKS copy of the collectors (namespace `kloudlite-git`, region
`central`, exporting to the gateway's ClusterIP) and five env vars on `kloudlite-git-admin`:
`KLOUDLITE_GIT_CLICKHOUSE_URL`/`_USER`/`_PASSWORD`, `KLOUDLITE_GIT_HYPERDX_URL` and
`KLOUDLITE_GIT_REGION`. The first four are optional — unset, the admin process behaves exactly as it
did and `/admin/history/*` answers 503. `KLOUDLITE_GIT_REGION` labels the watch against the mounted
kubeconfig's cluster and must equal that region's collector value (`centralindia-k3s`), or
telemetry and events land under different region names and neither side looks wrong.

## Release: the rename (2026-09-04)

`rustic-git` became `kloudlite-git` everywhere: crates, binaries, `KLOUDLITE_GIT_*`, the CRD group
`kloudlite-git.io` and its labels, images `ghcr.io/kloudlite/kloudlite-git{,-web}`, namespaces
`kloudlite-git` (AKS) and `kloudlite-git-system` (k3s), every Secret and ServiceAccount. Two things
deliberately kept their old name because they are data locations, not labels: the blob container
`rustic-git` (`KLOUDLITE_GIT_S3_URL=az://rustic-git`, every repo lives there) (the retired ZeroFS prefix inside it can be deleted). The ClickHouse database and user were renamed in place (`RENAME DATABASE`).

The cutover ran old and new side by side only for the stateless tiers. The server tier and the
agent are single-writer (the ownership lease, the btrfs pool), so the old StatefulSet and
DaemonSet were scaled to zero before the new ones took over. Objects owned by the old-group
CRs (tenant NetworkPolicies, ResourceQuotas) were garbage-collected with the old CRDs and
recreated by the new agent on its next reconcile; the four live CRs (Region, OwnerBindings) were
re-created by hand under the new group with the same names and specs. The api tier's k3s
kubeconfigs were re-minted for the new ServiceAccounts (`kloudlite-git-api`, `-admin`).
