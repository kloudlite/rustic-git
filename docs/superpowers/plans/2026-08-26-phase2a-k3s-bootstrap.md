# Phase 2A: k3s Cluster Bootstrap — Implementation Plan

> **For agentic workers:** execute this with superpowers:subagent-driven-development — one task per
> subagent, in order, each finishing with its stated verification output pasted back.

**Goal:** stand up a three-node k3s cluster (one tainted control plane, one session worker, one env
worker) that runs `kloudlite-git-agent` as a privileged DaemonSet against a host btrfs pool at
`/wspool-prod`, replacing the single Azure VM (20.219.39.174) where the agent runs as a plain
process — without touching that VM until rollback has been proven.

**Architecture:**

```
control-plane node  (2 OCPU / 8 GB)   k3s server, SQLite datastore, taint node-role.kubernetes.io/control-plane=true:NoSchedule
        |  6443 (API)  8472/udp (flannel VXLAN)  10250 (kubelet)
        +-- session node (32 OCPU / 128 GB)  label kloudlite-git.io/role=session  taint kloudlite-git.io/role=session:NoSchedule
        |     /wspool-prod  (dedicated data disk, btrfs)  -> agent DaemonSet pod (privileged, hostPath)
        +-- env node     (16 OCPU / 128 GB)  label kloudlite-git.io/role=env      taint kloudlite-git.io/role=env:NoSchedule
              /wspool-prod  (dedicated data disk, btrfs)  -> agent DaemonSet pod (privileged, hostPath)
```

The agent is a **node-level Kubernetes controller**: it watches kloudlite-git CRDs filtered to its own
node (`spec.nodeName`) and reconciles them against the local btrfs pool, instead of long-polling
`GET /vol-agent/work`. The Kubernetes API is the work queue. It still holds registry/Azure/Cosmos
credentials, because push/pull of snapshot bytes and commit history do not go through the API
server. This cluster holds no kloudlite-git server pods, no SlateDB, and no
part of the git/registry namespaces — only workspaces/environments compute. Nothing here is a
member of the app cluster; they are two clusters that talk over HTTPS with the peer/agent token.

**Tech Stack:** Ubuntu 24.04 LTS, k3s (SQLite datastore, embedded flannel VXLAN + kube-router
NetworkPolicy + CoreDNS), btrfs on a dedicated data disk, `az` CLI (Azure primary) / `oci` CLI
(Oracle alternate), plain YAML manifests in `deploy/` (no helm, no kustomize).

**Spec:** `docs/superpowers/specs/2026-08-26-k3s-architecture-design.md`

## Global Constraints

- **Never echo a node token, kubeconfig, or agent token into a log, transcript, or commit.** Move
  them with `scp`/`kubectl create secret --from-file`, redirect to a file with `>`, and verify with
  `wc -c` or `sha256sum | cut -c1-8` — never `cat`, never `echo $TOKEN`, never `kubectl get secret
  -o yaml` without `-o jsonpath` into a file. If a secret does reach a transcript, rotate it before
  continuing (`k3s token rotate` for the node token; re-issue the agent token from the server tier).
- Every manifest is committed under `deploy/` in this repo's existing style: one file per concern,
  comments explain WHY only, matching the density of `deploy/kloudlite-git.yaml`.
- Commit subjects imperative sentence case, no tool attribution.
- **Nothing destructive to the existing docker-based agent on 20.219.39.174 until Task 9's rollback
  is proven.** The old agent keeps running and keeps its pool the entire time; the k3s agents
  register as *additional* agents in a separate region (`WS_REGION=k3s`) so no job is stolen from
  the VM until cutover is deliberate.
- Node sizes and cloud are parameterized at the top of `deploy/k3s/env.example.sh`; only that file
  changes between Azure and Oracle.

## File Structure

| Path | Responsibility |
| --- | --- |
| `deploy/k3s/env.example.sh` | Parameters: cloud, region, VM sizes, disk sizes, node names, `WS_REGISTRY_URL`. Copied to a git-ignored `env.sh` and sourced by every script. No secrets. |
| `deploy/k3s/provision-azure.sh` | `az` commands creating the RG, VNet/subnet, NSG rules, three VMs, and one data disk per worker. Idempotent-ish; prints nothing secret. |
| `deploy/k3s/format-pool.sh` | Runs on a worker: formats the data disk btrfs, mounts `/wspool-prod`, writes the `/etc/fstab` UUID entry. |
| `deploy/k3s/install-server.sh` | k3s server install (SQLite) on the control plane, with the control-plane taint and disabled components. |
| `deploy/k3s/install-agent.sh` | k3s agent (worker) install with `--node-label`/`--node-taint` for its role. |
| `deploy/k3s/verify-cluster.sh` | The four cluster checks: nodes Ready, cross-node pod ping, CoreDNS Service resolution from the other node, NetworkPolicy actually denying. Exit 0 = cluster trustworthy. |
| `deploy/k3s/crds.yaml` | The CustomResourceDefinitions the agent controller watches (schemas come from the Rust-side plan; this file is the install artifact). |
| `deploy/k3s/agent-rbac.yaml` | Namespace `kloudlite-git-ws`, ServiceAccount `kloudlite-git-agent`, ClusterRole/Role + bindings: watch/list/get on the CRDs, update on their `/status`, and the pods/namespaces it manages. |
| `deploy/k3s/agent-daemonset.yaml` | The privileged agent DaemonSet (one per role, two specs in one file), hostPath mounts, env/secretRefs, liveness probe. |
| `deploy/k3s/netpol-env-namespace.yaml` | Default-deny ingress+egress template for an environment namespace, plus the DNS and registry-egress allowances. |
| `deploy/k3s/ROLLBACK.md` | The proven path back to the docker-based agent on the VM. |

---

### Task 10: Back up the control plane (do this BEFORE any real data lands)

With the CRDs as the source of truth and SQLite as the datastore, every workspace and environment
record is one file on one node. The btrfs subvolumes and the pushed blobs survive losing that node;
the record of what they ARE does not. Cosmos was managed and replicated — `state.db` is not, so the
replication is ours to do.

**Files:** `deploy/k3s/backup-controlplane.sh`

- [ ] **Step 1:** State the verification first — show there is no backup yet.
  `az storage blob list --account-name kloudlitegitkolomi -c k3s-backup -o tsv`
  Expected: the container does not exist, or lists nothing.
- [ ] **Step 2:** Create the container and a write-only SAS scoped to it (permissions `cwr`, no
  list, no delete — a compromised node must not be able to erase its own backups). Install it on the
  control plane at `/etc/kloudlite-git/k3s-backup.sas`, mode 600, root-owned. Never `cat` it.
- [ ] **Step 3:** Install `sqlite3` and `deploy/k3s/backup-controlplane.sh` as
  `/usr/local/bin/k3s-backup` (mode 750). The script uses `VACUUM INTO`, not `cp`: SQLite is in WAL
  mode and being written while the backup runs, so a plain copy can capture a torn page or miss
  committed transactions still in the WAL. It also archives `tls`, `token` and `cred` alongside the
  database — restoring `state.db` onto a k3s that generated a DIFFERENT cluster CA gives you a
  cluster no agent can join and no client can authenticate to. The database alone is not a
  restorable backup.
- [ ] **Step 4:** Run it once and verify the artifact is genuinely restorable, not merely uploaded:
  download the blob, unpack it, and read the database back.
  ```sh
  az storage blob download --account-name kloudlitegitkolomi -c k3s-backup -n daily-$(date -u +%a).tgz -f /tmp/r.tgz
  tar -xzf /tmp/r.tgz -C /tmp && sqlite3 /tmp/state.db "select count(*) from kine where name like '%kloudlite-git.io%';"
  ```
  Expected: a non-zero count. A backup that uploads but does not restore is worse than none, because
  it stops anyone looking for the problem.
- [ ] **Step 5:** Install `k3s-backup.timer` (`OnCalendar=hourly`, `Persistent=true` so a node that
  was off backs up on return). Rotation is by fixed names that overwrite — `hourly-{00..23}` and
  `daily-{Mon..Sun}` — which bounds storage without needing list or delete permission in the SAS.
- [ ] **Step 6:** Verify the timer is armed: `systemctl list-timers k3s-backup --no-pager`.
- [ ] **Step 7:** Commit: `Back up the k3s control plane hourly to Azure Blob`

The restore procedure lives in the script's own trailing comment, next to the code it applies to.
Its one non-obvious step: delete `state.db-wal`/`state.db-shm` before starting k3s, or a WAL from
the OLD database silently reintroduces the state you were rolling back.

**Known ceiling:** one server node means this is recovery, not availability — losing the control
plane still means downtime while a replacement is restored. `// ponytail: SQLite on one node;
upgrade path is embedded etcd across three servers when workspace metadata being down for the
length of a restore stops being acceptable.`

---

### Task 1: Parameters and provisioning scripts

**Files:** `deploy/k3s/env.example.sh`, `deploy/k3s/provision-azure.sh`

- [ ] **Step 1:** State the verification first — show the target does not exist yet.
  `az group show -n kloudlite-git-k3s -o tsv --query name`
  Expected: `ResourceGroupNotFound` on stderr, exit 3. Anything else means a previous run exists —
  stop and reconcile before creating.
- [ ] **Step 2:** Write `deploy/k3s/env.example.sh` with exactly these variables and defaults:
  ```sh
  # Copy to deploy/k3s/env.sh (git-ignored) and edit. NO SECRETS HERE — tokens go via scp/kubectl.
  CLOUD=azure                     # azure | oci  — only this file differs between them
  RG=kloudlite-git-k3s
  LOC=centralindia
  IMAGE=Canonical:ubuntu-24_04-lts:server:latest
  ADMIN=azureuser
  CP_SIZE=Standard_D2s_v5         # control plane: 2 OCPU / 8 GB, hosts no workloads
  SESSION_SIZE=Standard_E32ds_v5  # session worker: 32 OCPU / 128 GB
  ENV_SIZE=Standard_E16ds_v5      # env worker:     16 OCPU / 128 GB
  POOL_DISK_GB=1024               # per-worker dedicated data disk -> btrfs -> /wspool-prod
  CP=k3s-cp; SESSION=k3s-session; ENVN=k3s-env
  WS_REGISTRY_URL=https://git.khost.dev   # server tier's agent work surface (NOT bins/api)
  ```
  Verify: `bash -n deploy/k3s/env.example.sh && grep -ciE 'key|token|password' deploy/k3s/env.example.sh`
  Expected: exit 0 and `0`.
- [ ] **Step 3:** Add `deploy/k3s/env.sh` to `.gitignore`.
  `git check-ignore -v deploy/k3s/env.sh`
  Expected: a line naming `.gitignore` and the pattern.
- [ ] **Step 4:** Write `provision-azure.sh` — network first, so the NSG exists before any NIC:
  ```sh
  az group create -n "$RG" -l "$LOC"
  az network vnet create -g "$RG" -n k3s-vnet --address-prefix 10.60.0.0/16 \
    --subnet-name nodes --subnet-prefix 10.60.1.0/24
  az network nsg create -g "$RG" -n k3s-nsg
  # SSH from the operator only. Replace with your own /32 — 0.0.0.0/0 here is a finding, not a default.
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n ssh --priority 100 \
    --source-address-prefixes "$ADMIN_CIDR" --destination-port-ranges 22 --protocol Tcp --access Allow
  # Everything k3s needs is INTRA-cluster only: never expose 6443/10250/8472 to the internet.
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n k3s-api --priority 200 \
    --source-address-prefixes 10.60.1.0/24 --destination-port-ranges 6443 --protocol Tcp --access Allow
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n flannel-vxlan --priority 210 \
    --source-address-prefixes 10.60.1.0/24 --destination-port-ranges 8472 --protocol Udp --access Allow
  az network nsg rule create -g "$RG" --nsg-name k3s-nsg -n kubelet --priority 220 \
    --source-address-prefixes 10.60.1.0/24 --destination-port-ranges 10250 --protocol Tcp --access Allow
  az network vnet subnet update -g "$RG" --vnet-name k3s-vnet -n nodes --network-security-group k3s-nsg
  ```
  Verify: `az network nsg rule list -g "$RG" --nsg-name k3s-nsg -o tsv --query "[].{n:name,p:destinationPortRange}" | sort`
  Expected exactly four rows: `flannel-vxlan 8472`, `k3s-api 6443`, `kubelet 10250`, `ssh 22`.
- [ ] **Step 5:** Append the VM creations to the same script (control plane has no data disk):
  ```sh
  for spec in "$CP:$CP_SIZE:0" "$SESSION:$SESSION_SIZE:$POOL_DISK_GB" "$ENVN:$ENV_SIZE:$POOL_DISK_GB"; do
    IFS=: read -r name size disk <<<"$spec"
    az vm create -g "$RG" -n "$name" --image "$IMAGE" --size "$size" \
      --admin-username "$ADMIN" --ssh-key-values ~/.ssh/id_ed25519.pub \
      --vnet-name k3s-vnet --subnet nodes --nsg "" --public-ip-sku Standard
    [ "$disk" = 0 ] || az vm disk attach -g "$RG" --vm-name "$name" -n "$name-pool" \
      --new --size-gb "$disk" --sku Premium_LRS
  done
  ```
  `--nsg ""` matters: the subnet NSG is the single place rules live; a per-NIC NSG would silently
  shadow it. Verify: `az vm list -g "$RG" -d -o table` shows three VMs `VM running`, and
  `az vm show -g "$RG" -n "$SESSION" --query "storageProfile.dataDisks[].diskSizeGb" -o tsv` prints `1024`.
- [ ] **Step 6:** Note the OCI equivalent in a comment block at the bottom of the script, not a
  second script: `oci network vcn create` + `oci network security-list update` with the same four
  ingress rules scoped to the subnet CIDR, `oci compute instance launch --shape VM.Standard.E5.Flex
  --shape-config '{"ocpus":32,"memoryInGBs":128}'`, `oci bv volume create` + `volume-attachment
  create --type paravirtualized`. Everything after Task 1 is identical on both clouds.
  Verify: `grep -c '^# oci ' deploy/k3s/provision-azure.sh` ≥ 4.

### Task 2: btrfs pool on each worker

**Files:** `deploy/k3s/format-pool.sh`

- [ ] **Step 1:** Show the pool is absent. On each worker:
  `findmnt -n /wspool-prod || echo ABSENT`
  Expected: `ABSENT`.
- [ ] **Step 2:** Identify the data disk by size, never by `/dev/sdc` (device names reorder across
  reboots): `lsblk -dno NAME,SIZE,TYPE | awk '$3=="disk"'`
  Expected: the OS disk plus one `1T` disk — record that one as `$DEV`.
- [ ] **Step 3:** Write and run `format-pool.sh`:
  ```sh
  set -euo pipefail
  DEV=${1:?device, e.g. /dev/sdc}
  blkid "$DEV" && { echo "refusing: $DEV already has a filesystem" >&2; exit 1; }
  mkfs.btrfs -L wspool "$DEV"
  mkdir -p /wspool-prod
  UUID=$(blkid -s UUID -o value "$DEV")
  grep -q "$UUID" /etc/fstab || echo "UUID=$UUID /wspool-prod btrfs defaults,noatime 0 0" >> /etc/fstab
  systemctl daemon-reload && mount /wspool-prod
  ```
  The `blkid` guard is the whole safety of this script — it is why re-running it cannot eat a pool.
- [ ] **Step 4:** Verify the mount survives a remount and btrfs actually works:
  `mount -o remount /wspool-prod && btrfs subvolume create /wspool-prod/_probe && btrfs subvolume delete /wspool-prod/_probe && findmnt -no FSTYPE /wspool-prod`
  Expected: `Create subvolume`, `Delete subvolume`, then `btrfs`.
- [ ] **Step 5:** Reboot the node and re-check (fstab typos only show up here):
  `sudo reboot` then after SSH returns `findmnt -no TARGET,FSTYPE /wspool-prod`
  Expected: `/wspool-prod btrfs`.

### Task 3: k3s server on the control plane

**Files:** `deploy/k3s/install-server.sh`

- [ ] **Step 1:** Show it is absent: `systemctl is-active k3s || true`
  Expected: `inactive` (or `Unit k3s.service could not be found`).
- [ ] **Step 2:** Install with the SQLite datastore — SQLite is what you get by *not* passing
  `--datastore-endpoint` and *not* passing `--cluster-init`; there is no positive flag for it:
  ```sh
  curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION=v1.33.5+k3s1 sh -s - server \
    --node-taint node-role.kubernetes.io/control-plane=true:NoSchedule \
    --disable traefik --disable servicelb --disable local-storage \
    --write-kubeconfig-mode 600 \
    --tls-san "$CP_PUBLIC_IP"
  ```
  Traefik/servicelb/local-storage are disabled because this cluster serves no ingress and stores no
  PVs — the workspace pool is a hostPath on purpose. Flannel, CoreDNS and kube-router stay.
- [ ] **Step 3:** Verify the datastore really is SQLite and no etcd is running:
  `sudo ls /var/lib/rancher/k3s/server/db/ && sudo ls /var/lib/rancher/k3s/server/db/etcd 2>&1 | tail -1`
  Expected: `state.db` (plus `-wal`/`-shm`) present; the etcd listing fails with `No such file or directory`.
- [ ] **Step 4:** Verify the taint took:
  `sudo k3s kubectl get node -o jsonpath='{.items[0].spec.taints[*].key}{"\n"}'`
  Expected: contains `node-role.kubernetes.io/control-plane`.
- [ ] **Step 5:** Retrieve the node token WITHOUT printing it. On the control plane:
  `sudo install -m600 -o "$USER" -g "$USER" /var/lib/rancher/k3s/server/node-token ~/node-token && wc -c < ~/node-token`
  Expected: a byte count around 100-120. Copy to the workstation with
  `scp cp:~/node-token ./node-token` and `chmod 600 ./node-token`. **Do not cat it.**
- [ ] **Step 6:** Retrieve the kubeconfig the same way, rewriting the server address on the fly:
  ```sh
  ssh cp 'sudo cat /var/lib/rancher/k3s/server/k3s.yaml' > ~/.kube/kloudlite-k3s.yaml
  chmod 600 ~/.kube/kloudlite-k3s.yaml
  sed -i '' "s#127.0.0.1#$CP_PUBLIC_IP#" ~/.kube/kloudlite-k3s.yaml
  ```
  That `sudo cat` goes straight into a redirect, never onto a terminal. Verify without exposing it:
  `KUBECONFIG=~/.kube/kloudlite-k3s.yaml kubectl get --raw /version | head -c 60`
  Expected: a JSON `{"major":"1","minor":"33"...` fragment.
- [ ] **Step 7:** Confirm the API port is closed to the internet:
  `nc -z -w3 "$CP_PUBLIC_IP" 6443; echo $?` from a machine outside the VNet.
  Expected: non-zero (timeout). Step 6 works only from an allowed source — if it worked from
  anywhere, the NSG rule from Task 1 is wrong and must be fixed before continuing.

### Task 4: Join the two workers with labels and taints

**Files:** `deploy/k3s/install-agent.sh`

- [ ] **Step 1:** Show only one node exists:
  `kubectl get nodes -o name | wc -l`
  Expected: `1`.
- [ ] **Step 2:** Copy the token to each worker without printing it:
  `scp ./node-token session:~/node-token && ssh session 'chmod 600 ~/node-token'`
  Verify: `ssh session 'wc -c < ~/node-token'` matches Task 3 Step 5's count.
- [ ] **Step 3:** Join the session node (`K3S_TOKEN_FILE` keeps the token out of the process table
  and out of shell history, which `K3S_TOKEN=` would not):
  ```sh
  curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION=v1.33.5+k3s1 \
    K3S_URL="https://$CP_PRIVATE_IP:6443" K3S_TOKEN_FILE=$HOME/node-token sh -s - agent \
    --node-label kloudlite-git.io/role=session \
    --node-taint kloudlite-git.io/role=session:NoSchedule
  ```
- [ ] **Step 4:** Same for the env node with `kloudlite-git.io/role=env` in both flags.
- [ ] **Step 5:** Verify labels and taints landed (a label without its taint is the dangerous half —
  workloads would spread onto the wrong node):
  `kubectl get nodes -L kloudlite-git.io/role -o custom-columns=N:.metadata.name,ROLE:.metadata.labels.kloudlite-git\\.io/role,TAINTS:.spec.taints[*].key`
  Expected: three rows; `k3s-cp <none> node-role.kubernetes.io/control-plane`,
  `k3s-session session kloudlite-git.io/role`, `k3s-env env kloudlite-git.io/role`.
- [ ] **Step 6:** Shred the local token copies: `shred -u ./node-token && ssh session 'shred -u ~/node-token' && ssh env 'shred -u ~/node-token'`
  Verify: `ls ./node-token` → `No such file or directory`.

### Task 5: Verify the cluster actually works

**Files:** `deploy/k3s/verify-cluster.sh`

Every check below runs on the tainted worker nodes, so each test pod carries
`tolerations: [{operator: Exists}]`. A test that quietly lands on the control plane proves nothing.

- [ ] **Step 1:** Nodes Ready.
  `kubectl wait --for=condition=Ready node --all --timeout=120s`
  Expected: three `node/... condition met` lines.
- [ ] **Step 2:** Cross-node pod ping (flannel VXLAN over 8472/udp). Create one pod pinned to each
  worker with `nodeSelector` on the role label, then:
  `kubectl exec probe-session -- ping -c3 -W2 $(kubectl get pod probe-env -o jsonpath='{.status.podIP}')`
  Expected: `3 packets transmitted, 3 received, 0% packet loss`. Loss here is the NSG's 8472/udp
  rule, not the pods.
- [ ] **Step 3:** CoreDNS resolves a Service from the *other* node. Expose `probe-env` as a Service,
  then from the session pod:
  `kubectl exec probe-session -- nslookup probe-env.default.svc.cluster.local`
  Expected: an `Address:` line inside `10.43.0.0/16`, and a matching
  `kubectl exec probe-session -- wget -qO- probe-env.default.svc.cluster.local:80` returning the
  probe's body. Name resolution succeeding while the fetch fails means DNS is fine and flannel is not.
- [ ] **Step 4:** **NetworkPolicy actually denies — the check that matters.** Policy silently doing
  nothing looks exactly like policy working, so prove the negative in both directions. First
  re-confirm the fetch in Step 3 SUCCEEDS (the "before"), then apply:
  ```yaml
  apiVersion: networking.k8s.io/v1
  kind: NetworkPolicy
  metadata: { name: probe-deny, namespace: default }
  spec:
    podSelector: { matchLabels: { app: probe-env } }
    policyTypes: [Ingress]
  ```
  and re-run the fetch:
  `kubectl exec probe-session -- wget -qO- -T5 probe-env.default.svc.cluster.local:80; echo rc=$?`
  Expected: `wget: download timed out` and `rc=1`. **If it still returns the body, kube-router is
  not enforcing** — check `kubectl -n kube-system logs -l k8s-app=kube-router --tail=20` and that
  the server was not installed with `--disable-network-policy`. Do not proceed to Task 6 until this
  step fails the fetch; every namespace isolation in Task 7 depends on it.
- [ ] **Step 5:** Delete the policy, confirm the fetch works again (proves Step 4 measured the
  policy and not a flaky pod), then delete the probes.
  Expected: body returned; `kubectl get pod probe-session probe-env` → `NotFound`.
- [ ] **Step 6:** Fold Steps 1-5 into `verify-cluster.sh` with `set -euo pipefail` and a final
  `echo CLUSTER OK`. Re-run end to end.
  Expected: `CLUSTER OK`, exit 0. This script is the gate re-run after any node or k3s change.

### Task 6: Install the CRDs

**Files:** `deploy/k3s/crds.yaml`

The agent controller watches these; **they must exist and be Established before any agent pod
starts**, or the pod's watch fails at startup and the controller sits idle looking healthy. That
ordering is why this task precedes the DaemonSet. The CRD *schemas* (group, versions, fields,
`status` shape) are defined by the Rust-side plan — this task installs whatever that plan produces
and verifies the API actually serves it.

- [ ] **Step 1:** Show absence: `kubectl get crd -o name | grep kloudlite-git.io || echo NONE`
  Expected: `NONE`.
- [ ] **Step 2:** Take `crds.yaml` from the Rust-side plan's generated output. Every CRD must have
  `subresources: { status: {} }` (the controller writes only status — without the subresource,
  status updates are silently folded into spec and the RBAC split in Task 7 is meaningless) and,
  in each object's spec, a `nodeName` field, because the per-node field selector below indexes on it.
  Verify shape before applying:
  `kubectl apply --dry-run=server -f deploy/k3s/crds.yaml`
  Expected: one `created (server dry run)` line per CRD, no schema errors.
- [ ] **Step 3:** Apply and wait for establishment — `kubectl apply` returning is not the same as
  the API serving the type:
  `kubectl apply -f deploy/k3s/crds.yaml && kubectl wait --for=condition=Established crd --all --timeout=60s`
  Expected: `condition met` per CRD.
- [ ] **Step 4:** Verify the API really serves them, not just that the CRD object exists:
  `kubectl api-resources --api-group=kloudlite-git.io`
  Expected: a row per kind with its short name and `NAMESPACED` value; and
  `kubectl get --raw /apis/kloudlite-git.io/v1alpha1 | head -c 200` returns an `APIResourceList`
  including the `<kind>/status` resources.
- [ ] **Step 5:** Prove the per-node field selector works, because that is what scopes each
  controller. Create two throwaway objects, one with `nodeName: k3s-session`, one with
  `nodeName: k3s-env`, then:
  `kubectl get <kind> --field-selector spec.nodeName=k3s-session -o name | wc -l`
  Expected: `1`. If it returns `2` or errors with `field label not supported`, the CRD lacks a
  `selectableFields` entry for `spec.nodeName` (Kubernetes 1.31 requires it — arbitrary CRD field
  selectors do not work by default). Fix that in `crds.yaml` before continuing; without it every
  node's controller sees every node's work and two agents will race the same subvolume.
- [ ] **Step 6:** Delete the throwaway objects.
  `kubectl get <kind> -A -o name | wc -l` → `0`.

### Task 7: Agent RBAC and secrets

**Files:** `deploy/k3s/agent-rbac.yaml`

- [ ] **Step 1:** Show absence: `kubectl -n kloudlite-git-ws get sa kloudlite-git-agent`
  Expected: `Error from server (NotFound)`.
- [ ] **Step 2:** Write `agent-rbac.yaml`: `Namespace kloudlite-git-ws`, `ServiceAccount
  kloudlite-git-agent`, and:
  - a **ClusterRole `kloudlite-git-agent-crd`** — `apiGroups: [kloudlite-git.io]`, `resources: [<kinds>]`,
    `verbs: [get, list, watch]`, plus a second rule `resources: [<kinds>/status]`,
    `verbs: [get, update, patch]`. Cluster-scoped and no `create`/`delete`: a watch cannot be
    namespace-restricted below the namespaces the objects live in, and the controller's only write
    is status. The **per-node narrowing is the field selector in the informer, not RBAC** —
    Kubernetes RBAC cannot filter by field, so state that in a comment rather than pretending the
    Role scopes it. A ClusterRole with only get/list/watch/status-update is the narrowest that works.
  - a **Role in `kloudlite-git-ws`** granting `pods`, `pods/log`, `pods/exec`, `services`,
    `configmaps`, `secrets`, `networkpolicies` (`get,list,watch,create,delete`) — the workspace
    containers it runs.
  - a small **ClusterRole** with `namespaces: get,list,create,delete` and `networkpolicies:
    create,delete`, for the per-environment namespaces of Task 9 Step 6.
  Nothing else: the agent needs no nodes, no PVs, no RBAC objects, and no `create` on its own CRDs
  (the API server creates the work, the controller reconciles it). Comment WHY each verb is there.
- [ ] **Step 3:** Create the secrets from files, never from literals on the command line
  (`--from-literal` puts the value in shell history and the process table):
  ```sh
  umask 077
  printf %s "$AGENT_TOKEN" > /tmp/tok    # typed from a password manager, not echoed
  kubectl -n kloudlite-git-ws create secret generic kloudlite-git-agent \
    --from-file=token=/tmp/tok --from-file=azure-key=/tmp/azkey --from-file=cosmos-key=/tmp/ckey
  shred -u /tmp/tok /tmp/azkey /tmp/ckey
  ```
  Verify without revealing: `kubectl -n kloudlite-git-ws get secret kloudlite-git-agent -o jsonpath='{range $.data.*}{@}{"\n"}{end}' | wc -l`
  Expected: `3`.
- [ ] **Step 4:** Apply and verify the roles permit exactly what is intended, negatives included:
  ```sh
  SA=system:serviceaccount:kloudlite-git-ws:kloudlite-git-agent
  kubectl auth can-i watch <kind>.kloudlite-git.io --as=$SA -A          # yes
  kubectl auth can-i update <kind>.kloudlite-git.io/status --as=$SA -A  # yes
  kubectl auth can-i delete <kind>.kloudlite-git.io --as=$SA -A         # no
  kubectl auth can-i create pods --as=$SA -n kloudlite-git-ws           # yes
  kubectl auth can-i delete nodes --as=$SA                           # no
  ```
  Expected: `yes yes no yes no`, in that order. A `yes` on the two negatives means a wildcard crept
  into a rule.

### Task 8: Agent DaemonSet

**Files:** `deploy/k3s/agent-daemonset.yaml`, `deploy/k3s/netpol-env-namespace.yaml`

- [ ] **Step 1:** Show absence: `kubectl -n kloudlite-git-ws get ds`
  Expected: `No resources found`.
- [ ] **Step 2:** Write the DaemonSet — two DaemonSets in one file (`kloudlite-git-agent-session`,
  `kloudlite-git-agent-env`) rather than one with a broad selector, because the two roles get
  different `WS_CPU`/`WS_MEM_MB` and different tolerations. Each carries:
  - `nodeSelector: { kloudlite-git.io/role: session }` (resp. `env`) **and** a matching
    `tolerations` entry for that role's `NoSchedule` taint — the selector alone schedules nothing on
    a tainted node.
  - `hostPID: true`, `securityContext: { privileged: true }` on the container. btrfs
    `subvolume`/`send`/`receive` and the loop-mount path in `bins/agent/src/lib.rs` (`mount -o loop`
    for block-restored workspaces) need real host mount namespace access, so also
    `hostPath` `/dev` (for `/dev/loop*` and `/dev/btrfs-control`) and
    `mountPropagation: Bidirectional` on the pool mount — without Bidirectional, a subvolume the
    agent mounts inside the pod is invisible to the host and to docker.
  - volumes: `/wspool-prod` → `/wspool-prod` (hostPath `Directory`, Bidirectional), `/dev` → `/dev`,
    `/var/run/docker.sock` → same (the agent still shells `docker`/`docker compose` —
    `docker_stop_name`, `compose` in `lib.rs`; the containerd migration is Phase 2B's problem).
  - `serviceAccountName: kloudlite-git-agent` — without it the pod gets `default`, whose token can
    list nothing, and the controller's watch fails closed at startup (Step 5 below catches that).
  - env: **the final list comes from the Rust-side plan** — the agent no longer takes its work from
    `WS_REGISTRY_URL` long-polling (it watches the CRDs of Task 6 instead), but it still needs the
    registry/Azure/Cosmos credentials for pushing and pulling snapshot bytes and history, which
    never go through the API server. Ship what is known now and note the dependency in a comment:
    `WS_REGION=k3s` (see Global
    Constraints — a separate region so no work is taken from the VM before cutover), `WS_POOL=/wspool-prod`,
    `WS_CPU`/`WS_MEM_MB`/`WS_DISK_GB` per role, `HOSTNAME` from `fieldRef spec.nodeName` (the
    agent uses it as its identity; the pod's random name would change every roll),
    `WS_AGENT_TOKEN`/`AZURE_KEY`/`COSMOS_KEY` from the Task 7 secret, `AZURE_ACCOUNT`,
    `AZURE_CONTAINER`, `COSMOS_ENDPOINT`, `COSMOS_DB` from a ConfigMap. Keep `WS_REGISTRY_URL` set
    until the Rust-side plan confirms nothing reads it — an unused env var costs nothing, a missing
    one costs a CrashLoop.
  - `updateStrategy: { type: OnDelete }`: a rolling DaemonSet update would kill an agent
    mid-`btrfs send`. Roll a node deliberately, after its jobs drain.
  - liveness probe: the agent has **no HTTP port** today (`lib.rs` is a bare poll loop), so use an
    exec probe on a heartbeat file, the same shape as the worker's per-lane heartbeat files:
    `exec: { command: ["sh","-c","test $(( $(date +%s) - $(stat -c %Y /wspool-prod/.agent-heartbeat) )) -lt 120"] }`
    with `initialDelaySeconds: 60`, `periodSeconds: 30`. **The agent does not write that file yet —
    the Rust change is Phase 2B.** Until it lands, ship the probe commented out with a
    `# ponytail: no heartbeat file until Phase 2B; a wedged poll loop is not detected — uncomment
    once bins/agent writes it` marker, rather than a probe that would CrashLoop every pod.
- [ ] **Step 3:** Apply and verify placement — exactly one pod per worker, none on the control plane:
  `kubectl -n kloudlite-git-ws get pod -o custom-columns=N:.metadata.name,NODE:.spec.nodeName,S:.status.phase`
  Expected: two rows, `Running`, on `k3s-session` and `k3s-env` — never `k3s-cp`.
- [ ] **Step 4:** Verify the pool is genuinely the host's, not an empty ephemeral dir:
  `kubectl -n kloudlite-git-ws exec ds/kloudlite-git-agent-session -- btrfs subvolume list /wspool-prod`
  and compare with `ssh session 'sudo btrfs subvolume list /wspool-prod'`.
  Expected: identical listings. Differing (especially an empty pod-side list) means the hostPath
  mounted over a fresh directory — check the `type: Directory` and that Task 2's mount is up.
- [ ] **Step 5:** **Verify each pod can reach the API server AND actually establish its watch.** A
  controller whose watch never establishes looks exactly like a controller with no work to do —
  the same silent-success failure as an unenforced NetworkPolicy, so prove it positively:
  ```sh
  # a) the SA token works from inside the pod at all
  kubectl -n kloudlite-git-ws exec ds/kloudlite-git-agent-session -- \
    sh -c 'wget -qO- --no-check-certificate --header="Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)" \
      https://kubernetes.default.svc/apis/kloudlite-git.io/v1alpha1 | head -c 120'
  # b) the controller says so itself
  kubectl -n kloudlite-git-ws logs ds/kloudlite-git-agent-session --tail=30
  ```
  Expected: (a) an `APIResourceList` fragment naming the CRD kinds — a `401`/`403` body means the
  ServiceAccount or Task 7's ClusterRole is wrong, an empty body means NetworkPolicy or DNS; (b) a
  log line stating the watch is established for each kind, naming node `k3s-session`, and no token
  value anywhere in the output. If a token appears in a log line, that is a code bug to fix in the
  Rust-side plan before this reaches production.
- [ ] **Step 5b:** Prove the watch is **scoped to its own node** — otherwise two agents race the
  same subvolume. Create one CRD object with `nodeName: k3s-session`, watch both pods' logs:
  `kubectl -n kloudlite-git-ws logs ds/kloudlite-git-agent-env --tail=10`
  Expected: the session pod logs a reconcile for the object; the env pod logs **nothing** about it.
  The env pod reacting is Task 6 Step 5's `selectableFields` missing, or the informer built without
  the field selector. Delete the object afterwards.
- [ ] **Step 5c:** Prove the ordering constraint bites, once, so nobody re-discovers it in
  production: `kubectl delete -f deploy/k3s/crds.yaml` then delete a pod and read its logs.
  Expected: the pod fails or logs `the server could not find the requested resource` rather than
  running idle. Re-apply `crds.yaml`, delete the pod again, confirm the watch re-establishes per
  Step 5. **CRDs before DaemonSet, always** — record it in `ROLLBACK.md`.
- [ ] **Step 6:** Namespace convention + default deny. Environments get one namespace each,
  `env-{id}`; workspaces stay in `kloudlite-git-ws`. Write `netpol-env-namespace.yaml` as a template
  with `namespace: env-PLACEHOLDER`: a `default-deny` policy (`podSelector: {}`,
  `policyTypes: [Ingress, Egress]`), an `allow-dns` egress to `kube-system`'s CoreDNS on 53/UDP+TCP,
  an `allow-same-namespace` ingress+egress from `podSelector: {}` (services within one environment
  must talk), and an `allow-registry-egress` to the server tier's CIDR/port. No ingress from other
  namespaces at all — an environment reaching another tenant's environment is the failure this
  prevents.
- [ ] **Step 7:** Prove the template denies, in a throwaway namespace, the same before/after shape
  as Task 5 Step 4: create `env-probe`, run a pod, `wget` a pod in `kloudlite-git-ws` (succeeds),
  apply the rendered template, re-run.
  Expected: `download timed out`, rc=1 — and DNS still resolving
  (`nslookup kubernetes.default` succeeds), which is what proves `allow-dns` is scoped right and
  not that everything is simply broken. Then `kubectl delete ns env-probe`.

### Task 9: Prove the rollback before cutover

**Files:** `deploy/k3s/ROLLBACK.md`

The old VM's agent has been running untouched this whole time; rollback is therefore mostly
*proving* that, not restoring it.

- [ ] **Step 1:** Confirm the old agent is still healthy and still serving its own region:
  `ssh 20.219.39.174 'systemctl is-active kloudlite-git-agent && journalctl -u kloudlite-git-agent --since -5m --no-pager | tail -5'`
  Expected: `active`, recent long-poll lines. If this is not true, the Global Constraint was
  violated somewhere — stop and restore before continuing.
- [ ] **Step 2:** Rehearse the rollback for real: scale both DaemonSets to zero by adding a
  `nodeSelector` that matches nothing (DaemonSets have no `replicas`):
  `kubectl -n kloudlite-git-ws patch ds kloudlite-git-agent-session -p '{"spec":{"template":{"spec":{"nodeSelector":{"kloudlite-git.io/role":"disabled"}}}}}'`
  (and the same for `-env`).
  Expected: `kubectl -n kloudlite-git-ws get pod` → `No resources found` within ~30s.
- [ ] **Step 3:** With k3s agents gone, create a workspace against the OLD region and prove it still
  works end to end — the actual rollback assertion:
  `WS_REGION=<vm-region> ./tests/ws_e2e.sh` on the VM (or the equivalent create+push+clone via
  `/v1/workspaces` against `bins/api`).
  Expected: exit 0. Exit 77 does not count as a pass — the VM has btrfs and docker, so a skip means
  the environment regressed and must be fixed first.
- [ ] **Step 4:** Restore the DaemonSets (`kubectl patch` the nodeSelector back to the real role
  values) and re-run `deploy/k3s/verify-cluster.sh` plus Task 8 Step 3.
  Expected: `CLUSTER OK` and two Running pods.
- [ ] **Step 5:** Write `ROLLBACK.md` recording exactly Steps 1-4 as the procedure, with the one
  irreversible caveat stated plainly: **data written to a k3s node's `/wspool-prod` is not on the
  VM**. Anything pushed from the k3s region is safe (content-addressed blobs in Azure, history in
  Cosmos — `restore` can graft it anywhere), but un-pushed local workspace state on a k3s node is
  lost by a rollback. Cutover therefore drains by pushing, not by copying subvolumes.
  Verify: `grep -c 'un-pushed' deploy/k3s/ROLLBACK.md` ≥ 1.
- [ ] **Step 6:** Commit everything:
  `git add deploy/k3s && git commit -m "Add k3s cluster bootstrap manifests and scripts"`
  Expected: one commit, no tool attribution in the message, `git show --stat` listing the eleven files
  from **File Structure**.
