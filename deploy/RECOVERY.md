# Recovery: rebuilding everything from what survives

What this is for: both clusters are gone, or one is, and the only things left are the Azure
services (`deploy/BACKUPS.md` says which switches must have been on for that to be true), this
repository, and the password manager. Every step below is ordered; the order is load-bearing
where it says so. Verification is inline — do not move on past a verify line that fails.

What survives a cluster loss, and is the input to this page:

| Have | Where | Without it |
| --- | --- | --- |
| Blob container `rustic-git` (SlateDB, packs, registry, credentials, `index/`) | storage account (Secret `rustic-git-storage` named it; the portal still does) | nothing to recover — this is the product |
| `wslayers*` containers, one per region | per-region storage accounts | workspace history is gone; live subvolumes on surviving pool nodes are not |
| Cosmos `rustic-git-mongo` (directory, PRs) and `rustic-git-cosmos` (`Region` rows) | Cosmos accounts | re-create `Region` rows by hand (Part C); the directory cannot be rebuilt |
| `k3s-backup` container (hourly encrypted control-plane bundles) | same account as `rustic-git` | the k3s region is rebuilt from `objects.yaml`-less scratch: subvolumes on the pool are orphans until re-adopted |
| `/etc/rustic-git/k3s-backup.key` copy | password manager | every bundle in `k3s-backup` is noise |
| Cloudflare zones `kloudlite.io`, `khost.dev`; GitHub org (GHCR packages, Actions) | their consoles | — |

Nothing else needs to exist beforehand. Every Secret is re-minted below; the values that cannot
be re-minted (storage/Cosmos keys) come from the portal.

Alerts to re-arm as each tier comes back: `deploy/alerts.md`. The backup switches to re-verify
once the clusters serve: `deploy/BACKUPS.md`.

## Order, in one screen

1. **k3s control plane** (Part B.1–B.3) — first, because the AKS api tier needs its
   ServiceAccount token, and nothing on AKS depends on the region's agents being up.
2. **AKS tier** (Part A) — Secrets, ingress, roll, Cloudflare.
3. **Region record + agent token** (Part C) — needs the api tier serving.
4. **k3s workloads** (Part B.4–B.7) — agent Secret, DaemonSet, gateway, NSG, hardening, backup timer.

A single-cluster loss is the same list minus the surviving half; the cross-cluster items are
marked **(cross)**.

---

## Part A — the AKS tier

### A.1 Cluster prerequisites

```sh
az aks get-credentials -g <rg> -n <cluster>
kubectl create namespace rustic-git
# ingress-nginx, Helm, defaults. cert-manager, Helm, with CRDs. Then the issuer the registry
# Ingress names (`cert-manager.io/cluster-issuer: letsencrypt` in deploy/rustic-git.yaml):
kubectl apply -f - <<'EOF'
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata: { name: letsencrypt }
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: <ops address>
    privateKeySecretRef: { name: letsencrypt }
    solvers: [{ http01: { ingress: { ingressClassName: nginx } } }]
EOF
```

Then the two ingress-nginx pieces this repo owns, both server-side because the objects are
Helm's (`deploy/ingress-nginx-origin-lock.md` is the why):

```sh
kubectl apply --server-side --force-conflicts -f deploy/ingress-nginx-config.yaml    # real client IPs
kubectl apply --server-side --force-conflicts -f deploy/ingress-nginx-service.yaml   # Cloudflare-only origin
# verify — non-empty and equal to the file, or every per-IP rate limit trusts a forgeable header:
kubectl -n ingress-nginx get svc ingress-nginx-controller -o jsonpath='{.spec.loadBalancerSourceRanges}'
```

Redis is a managed instance (Azure Managed Redis), not a manifest. It MUST run
`maxmemory-policy volatile-lru` — the comment above the `rustic-git-api` Service in
`deploy/rustic-git.yaml` explains why an `allkeys-*` policy silently un-purges caches. Create
it, set the policy, keep the connection URL for `rustic-git-redis` below.

### A.2 Secrets

All in namespace `rustic-git`. "Minted" means the value is invented here; "portal" means it
exists in Azure and is copied. A `(cross)` Secret must hold the same value in both clusters.

| Secret | Keys | Value | Consumers | Optional |
| --- | --- | --- | --- | --- |
| `rustic-git-storage` | `account`, `key` | portal: `az storage account keys list -n <acct> --query '[0].value' -o tsv` | srv, api, worker | no |
| `rustic-git-jwt` **(cross)** | `secret` | minted: `openssl rand -hex 32`. Copied verbatim into k3s `rustic-git-system` (B.5) — the gateway verifies what the api mints | srv, api, k3s gateway | no — pods fail closed without it |
| `rustic-git-peer` | `secret` | minted: `openssl rand -hex 32` | srv, api, worker, web | no |
| `rustic-git-hostkey` | `host_key` | minted: `ssh-keygen -q -t ed25519 -N '' -f host_key` (the file, not `.pub`). One key fleet-wide — per-pod keys make every pod a different host. **Every user's `known_hosts` entry for `git.khost.dev` changes**; announce it | srv | no |
| `rustic-git-mongo` | `uri` | portal: `az cosmosdb keys list -n rustic-git-mongo -g <rg> --type connection-strings` | srv, api, worker | no on srv (PRs orphan) |
| `rustic-git-redis` | `url` | the managed instance's `rediss://` URL with its access key | srv, api, worker | yes — slower, never wrong |
| `rustic-git-cosmos` | `endpoint`, `key`, `vol-agent-token` | portal: `az cosmosdb show -n rustic-git-cosmos --query documentEndpoint`, `az cosmosdb keys list --type keys`; `vol-agent-token` minted: `openssl rand -hex 32` — the break-glass list `RUSTIC_GIT_VOL_AGENT_TOKENS` on the server tier, distinct from any region's token | srv, api | yes — region routes 503 |
| `rustic-git-k3s-kubeconfig` **(cross)** | `config` | a kubeconfig whose user is the k3s `rustic-git-api` ServiceAccount token — B.3 mints it | api | no — /v1 workspace routes 503 |
| `rustic-git-web` | `auth-secret` (required); `github-id`/`-secret`, `google-id`/`-secret`, `allowed-emails`, `shared-password` (optional) | `auth-secret` minted: `openssl rand -hex 32`; OAuth values from the provider consoles (callback `https://dev.kloudlite.io/api/auth/callback/<provider>`) | web | providers optional |
| `rustic-git-mail` | `resend-api-key`, `from` | Resend console | web | yes — invites shown as links |

```sh
kubectl -n rustic-git create secret generic rustic-git-storage --from-literal=account=<acct> --from-literal=key=<key>
kubectl -n rustic-git create secret generic rustic-git-jwt   --from-literal=secret=$(openssl rand -hex 32)
kubectl -n rustic-git create secret generic rustic-git-peer  --from-literal=secret=$(openssl rand -hex 32)
ssh-keygen -q -t ed25519 -N '' -f host_key && kubectl -n rustic-git create secret generic rustic-git-hostkey --from-file=host_key && rm host_key host_key.pub
kubectl -n rustic-git create secret generic rustic-git-mongo --from-literal=uri='<connection string>'
kubectl -n rustic-git create secret generic rustic-git-redis --from-literal=url='rediss://:<key>@<host>:10000'
kubectl -n rustic-git create secret generic rustic-git-cosmos --from-literal=endpoint=<url> --from-literal=key=<key> --from-literal=vol-agent-token=$(openssl rand -hex 32)
kubectl -n rustic-git create secret generic rustic-git-k3s-kubeconfig --from-file=config=<kubeconfig from B.3>
kubectl -n rustic-git create secret generic rustic-git-web --from-literal=auth-secret=$(openssl rand -hex 32) [--from-literal=github-id=... ...]
kubectl -n rustic-git create secret generic rustic-git-mail --from-literal=resend-api-key=... --from-literal=from='...'
```

Verify: `kubectl -n rustic-git get secrets` lists all ten. Nothing else creates them.

### A.3 Apply and roll

The manifests are pinned to image SHAs that exist in GHCR (`deploy/pin.sh` refuses one that
does not); a fresh cluster pulls them anonymously. Values that name the environment and may
need editing on a rebuilt one, all in `deploy/rustic-git.yaml`: `RUSTIC_GIT_AGENT_SOURCES` and
the registry Ingress `limit-whitelist` (the pool nodes' public IPs — B.1 gives the new ones),
`RUSTIC_GIT_WORKSPACES_ADMINS`.

```sh
deploy/roll.sh          # one apply; the srv StatefulSet elects its own map writer
```

Verify, in this order (each is a different failure):

```sh
kubectl -n rustic-git get pods                                   # all Running, 0 restarts
kubectl -n rustic-git logs -l role=server --tail=500 | grep -E 'lease: leading|newer DB client'
#   want exactly ONE pod logging "lease: leading" (and "opened as WRITER" beside it), and NO
#   "newer DB client" on a settled fleet — that line means a demoted leader wrote after it lost
#   the lease, which the epoch check is supposed to stop first
kubectl -n rustic-git get endpoints rustic-git-lb -o jsonpath='{range .subsets[*].addresses[*]}{.targetRef.name}{"\n"}{end}'
#   every srv pod
kubectl -n rustic-git get svc rustic-git-lb -o jsonpath='{.status.loadBalancer.ingress[0].ip}'  # → git.khost.dev (A.4)
kubectl -n ingress-nginx get svc ingress-nginx-controller -o jsonpath='{.status.loadBalancer.ingress[0].ip}'  # → the two proxied names
```

### A.4 Cloudflare

Two zones, three HTTP names in two SSL modes plus one DNS-only SSH name. The mode per hostname
is what the yaml comments were written against; a wrong mode is a redirect loop
(`ERR_TOO_MANY_REDIRECTS`, or the 308 loop that took the registry down on 2026-08-23).

| Record | Points at | Proxied | Origin TLS | Why |
| --- | --- | --- | --- | --- |
| `dev.kloudlite.io` A | ingress-nginx LB IP | yes | none — SSL mode **Flexible**; the Ingress has `ssl-redirect: "false"` | `deploy/rustic-git-web.yaml`, comment on the Ingress |
| `cr.khost.dev` A | ingress-nginx LB IP | yes (today — `dig` resolves to Cloudflare) | cert-manager cert at the origin, but the proxy fetches over HTTP; `ssl-redirect: "false"` for that reason | `deploy/rustic-git.yaml`, registry Ingress comment. If it ever goes DNS-only, flip the annotation to `"true"` |
| `git.khost.dev` A | `rustic-git-lb` IP (SSH on 22, 2222 inside) | **no** — SSH cannot traverse the proxy | n/a | `deploy/rustic-git-web.yaml`, the SSH host comment |
| `ws-<region>.khost.dev` A ×N | each pool node's public IP | yes | Flexible today; Full (strict) needs the optional `gateway-tls` (B.5) | `deploy/k3s/gateway.yaml` header |

Zone SSL/TLS mode is per zone, not per record: `kloudlite.io` Flexible; `khost.dev` currently
Flexible (both `cr` and `ws-*` origins are fetched over HTTP). Moving `khost.dev` to Full
(strict) is one change with two prerequisites — the registry's cert-manager cert already
satisfies `cr`; `ws-*` needs an Origin CA cert in `gateway-tls` plus `GATEWAY_TLS_DIR` on the
gateway. Do not flip the mode before both hold.

The origin lock (A.1) and the pool NSG rule (B.6) both encode Cloudflare's ranges; they are
generated from `deploy/k3s/cloudflare-ips-v4.txt` by `deploy/cf-sync.sh`, never typed.

### A.5 Verify the tier serves

```sh
curl -s -o /dev/null -w 'git=%{http_code}\n'      https://dev.kloudlite.io/<owner>/<repo>.git/info/refs?service=git-upload-pack   # 401 or 200
curl -s -o /dev/null -w 'registry=%{http_code}\n' https://cr.khost.dev/v2/                                                    # 401 = auth challenge = healthy
curl -s -o /dev/null -w 'web=%{http_code}\n'      https://dev.kloudlite.io/api/health                                         # 200
ssh -T git@git.khost.dev                                                                                                      # host key = the one minted in A.2
git ls-remote https://dev.kloudlite.io/<owner>/<repo>.git                                                                      # a repo that existed: its refs came back from the object store
docker login cr.khost.dev && docker pull cr.khost.dev/<owner>/<image>:<tag>                                                    # layers came back from blobs/
# after ~5 min, the ownership map's WAL health:
kubectl -n rustic-git logs -l role=server | grep -iE 'checkpoint|timed out' | tail -3        # want "checkpoint ok in <ms>"
```

The first registry request to any image after a roll can 500 once (known fenced-handle gap);
a second one that also fails is real.

---

## Part B — the k3s region

Files and their order are the table at the top of `deploy/k3s/README.md`; this page only adds
what a REBUILD needs that a first build did not.

### B.1 Nodes

```sh
cp deploy/k3s/env.example.sh deploy/k3s/env.sh   # edit: names, sizes, ADMIN_CIDR
deploy/k3s/provision-azure.sh                    # idempotent; VNet, NSG (ssh + intra-VNet only), 3 VMs
# on each pool node:
ssh azureuser@<node> sudo bash -s -- /dev/disk/azure/scsi1/lun0 < deploy/k3s/format-pool.sh
```

Note the pool nodes' new public IPs: they go into `RUSTIC_GIT_AGENT_SOURCES` and the registry
`limit-whitelist` in `deploy/rustic-git.yaml` (A.3) — the server honours a region's agent token
only from those addresses.

### B.2 The control plane — from the backup, or from scratch

Same k3s version as the bundle (`v1.33.5+k3s1`; `kube` 4.2 needs ≥ v1.32). The restore path is
the trailing comment of `deploy/k3s/backup-controlplane.sh`; in short:

```sh
# on the new k3s-cp — install without starting a NEW cluster:
curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION=v1.33.5+k3s1 INSTALL_K3S_SKIP_START=true sh -s - server
# the key from the password manager, the newest bundle with a READ SAS (the backup SAS cannot read):
sudo install -d -m700 /etc/rustic-git && sudo sh -c 'umask 077; cat > /etc/rustic-git/k3s-backup.key'
echo 'url = "https://<acct>.blob.core.windows.net/k3s-backup/hourly-<HH>.tgz.enc?<read SAS>"' > /tmp/get.cfg
curl -sS --fail -K /tmp/get.cfg -o /tmp/b.tgz.enc
openssl enc -d -aes-256-cbc -pbkdf2 -pass file:/etc/rustic-git/k3s-backup.key -in /tmp/b.tgz.enc -out /tmp/b.tgz && tar -xzf /tmp/b.tgz -C /tmp
sudo install -m600 /tmp/state.db /var/lib/rancher/k3s/server/db/state.db
sudo rm -f /var/lib/rancher/k3s/server/db/state.db-wal /var/lib/rancher/k3s/server/db/state.db-shm   # the step that bites
sudo tar -xzf /tmp/identity.tgz -C /var/lib/rancher/k3s/server
sudo systemctl start k3s
```

`identity.tgz` restores the cluster CA and join token, so the node token from the old cluster
still joins workers. Workers: the k3s agent install with that token, then the labels:

```sh
kubectl label node <node> rustic-git.io/pool=true rustic-git.io/session=true   # or env=true
```

**No bundle, or a bundle the key cannot open** — a fresh cluster: install normally, then only
the YAML dump is usable, and only onto a cluster that already has the CRDs:

```sh
kubectl apply -f deploy/k3s/crds.yaml && kubectl apply -f /tmp/objects.yaml    # status is not restorable; controllers rebuild it
```

Without even `objects.yaml`, the subvolumes on a surviving pool are orphans: each owner's
history is still in `wslayers*` and the server tier's `vol/{owner}/{id}` records, so a workspace
is recreated through `/v1/workspaces/restore` from its last pushed snapshot, not adopted.

Verify: `kubectl get nodes` (all Ready, labels present); `kubectl get volumes,workspaces,environments`
(the objects from the bundle, `status.nodeName` set once the agent runs in B.5).

### B.3 The api tier's kubeconfig **(cross)**

```sh
kubectl apply -f deploy/k3s/crds.yaml -f deploy/k3s/api-rbac.yaml
TOKEN=$(kubectl -n kube-system create token rustic-git-api --duration=8760h)
CA=$(kubectl config view --raw --minify -o jsonpath='{.clusters[0].cluster.certificate-authority-data}')
cat > k3s-api.kubeconfig <<EOF
apiVersion: v1
kind: Config
clusters: [{ name: k3s, cluster: { server: https://<k3s-cp public IP>:6443, certificate-authority-data: $CA } }]
users: [{ name: rustic-git-api, user: { token: $TOKEN } }]
contexts: [{ name: k3s, context: { cluster: k3s, user: rustic-git-api } }]
current-context: k3s
EOF
```

That file is `rustic-git-k3s-kubeconfig`'s `config` key (A.2); delete the local copy after.
`harden-node.sh`'s `API_CLIENTS` (B.6) must include the AKS api tier's egress IP or 6443 is
closed to it. The token has an expiry — put a reminder where the JWT rotation one lives
(`deploy/BACKUPS.md`, "Rotation").

### B.4 RBAC, admission, nix

```sh
kubectl apply -f deploy/k3s/agent-rbac.yaml -f deploy/k3s/agent-admission.yaml -f deploy/k3s/nix-conf.yaml
```

### B.5 Secrets and the agent

The agent Secret is the one `deploy/k3s/README.md` says is "not in this directory". Keys, all
in `kube-system/rustic-git-agent`:

| Key | Value |
| --- | --- |
| `WS_REGISTRY_URL` | `https://cr.khost.dev` — the SERVER tier, not the api |
| `WS_REGION` | the region id (`centralindia-k3s`); must equal the gateway ConfigMap's |
| `WS_AGENT_TOKEN` | the region's token — Part C mints it; use a placeholder until then |
| `AZURE_ACCOUNT`, `AZURE_KEY`, `AZURE_CONTAINER` | this region's `wslayers*` account (portal) |
| `WS_RUNTIME_CLASS` | only `gvisor`, only once every pool node has it (comment in `agent-daemonset.yaml`) |

```sh
kubectl -n kube-system create secret generic rustic-git-agent --from-literal=WS_REGISTRY_URL=https://cr.khost.dev \
  --from-literal=WS_REGION=centralindia-k3s --from-literal=WS_AGENT_TOKEN=placeholder \
  --from-literal=AZURE_ACCOUNT=... --from-literal=AZURE_KEY=... --from-literal=AZURE_CONTAINER=wslayers-k3s
kubectl apply -f deploy/k3s/agent-daemonset.yaml
kubectl -n kube-system rollout status ds/rustic-git-agent
# (cross) the gateway verifies session tokens with the api's key:
kubectl -n rustic-git get secret rustic-git-jwt -o yaml | sed 's/namespace: rustic-git/namespace: rustic-git-system/' \
  | KUBECONFIG=.local/k3s.yaml kubectl apply -f -            # after gateway.yaml created the namespace, or create it first
# optional, only for Full (strict) on ws-*: Cloudflare → SSL/TLS → Origin Server → Create Certificate
kubectl -n rustic-git-system create secret tls gateway-tls --cert=<cert> --key=<key>
kubectl apply -f deploy/k3s/gateway.yaml
```

Verify: `kubectl -n kube-system logs -l app=rustic-git-agent --tail=50 | grep -E 'migration:|error'`
— the startup migration adopts every Volume it finds on the pool; then
`kubectl get workspaces -o custom-columns=NAME:.metadata.name,NODE:.status.nodeName` shows a
node on every row. Pushes fail at the registry until Part C replaces the placeholder token.

### B.6 Network: NSG rule, node firewall

The pool NSG admits 80 from Cloudflare only, and `provision-azure.sh` does not create that rule
(`deploy/k3s/README.md`, Gateway step 5):

```sh
az network nsg rule create -g rustic-git-k3s --nsg-name k3s-nsg -n gateway-cloudflare --priority 120 \
  --direction Inbound --access Allow --protocol Tcp --destination-port-ranges 80 \
  --source-address-prefixes $(tr '\n' ' ' < deploy/k3s/cloudflare-ips-v4.txt)
```

Then `harden-node.sh` on EVERY node, with all three variables — a run that omits one refuses
rather than silently opening or closing a port:

```sh
CF_CIDRS="$(paste -sd, deploy/k3s/cloudflare-ips-v4.txt)"
ssh azureuser@<node> "sudo CF_CIDRS='$CF_CIDRS' ADMIN_CIDR='<operator cidr>' API_CLIENTS='<AKS api egress IP>/32' bash -s" < deploy/k3s/harden-node.sh
```

Verify from outside Cloudflare: `curl -m 5 http://<pool node IP>/` times out; from the operator
CIDR `ssh` still works; `KUBECONFIG=k3s-api.kubeconfig kubectl get workspaces` works from
inside an AKS api pod (`kubectl -n rustic-git exec deploy/rustic-git-api -- ...` has no kubectl —
the /v1 route test in Part C is the real check). gVisor, if the region had it:
`deploy/k3s/install-gvisor.sh` per node, then the three enabling steps it prints.

### B.7 The backup timer

Re-install it exactly as `deploy/k3s/README.md` "Control-plane backup" says: a new
create+write SAS in `/etc/rustic-git/k3s-backup.sas`, the snitch URL in `k3s-backup.env`, the
SAME key from the password manager in `k3s-backup.key` (a new key would strand every older
bundle — generate a new one only if the old is compromised, and then keep both in the vault),
then the units. Verify: `systemctl list-timers backup-controlplane.timer` and a fresh
`hourly-*.tgz.enc` in the container within the hour.

---

## Part C — the region record and the agent token

`rustic-git-cosmos` (Core API, db `workspaces`) holds one `Region` row per region and nothing
else. If the account survived, the row is there and only the token needs rotating; if not,
re-register. Both go through the api tier (A.3 must be serving) as a workspaces admin
(`RUSTIC_GIT_WORKSPACES_ADMINS`):

```sh
ADMIN_JWT=<session token of an admin, from the web app's cookie or `kl` login>
# re-register (only when the row is gone; re-registering an existing id is also how one is retired):
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_JWT" -H 'Content-Type: application/json' https://dev.kloudlite.io/v1/regions \
  -d '{"id":"centralindia-k3s","name":"Central India (k3s)","storage_account":"<wslayers account>","blob_container":"wslayers-k3s"}'
# mint the agent token and install it in the region in one step (replaces the B.5 placeholder):
ADMIN_JWT=$ADMIN_JWT deploy/k3s/rotate-agent-token.sh centralindia-k3s .local/k3s.yaml
```

Verify end to end — this is the only check that exercises both clusters and Cosmos together:

```sh
# "Open in a workspace" on a repository in the web app, or the same two calls the CLI cannot make yet:
ID=$(curl -fsS -X POST -H "Authorization: Bearer $ADMIN_JWT" -H 'Content-Type: application/json' https://dev.kloudlite.io/v1/workspaces \
  -d '{"name":"recovery-check","region":"centralindia-k3s","quota_gb":10}' | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
KUBECONFIG=.local/k3s.yaml kubectl wait workspace/$ID --for=condition=Ready --timeout=10m   # claimed, materialized, pod up
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_JWT" -H 'Content-Type: application/json' \
  https://dev.kloudlite.io/v1/workspaces/$ID/push -d '{"message":"recovery check"}'
KUBECONFIG=.local/k3s.yaml kubectl get snapshots          # the new one reaches phase ready = agent token, wslayers creds and the server's vol/ surface all right
kl ws ssh $ID -- true                                      # the gateway: DNS, NSG, harden-node, the copied jwt Secret
```

---

## What is still manual after all of this

- `RUNBOOK.local.md` (git-ignored) holds cluster-specific commands with real names in them; the
  procedures above are its non-secret content. Anything that only exists there is a gap in this
  page — move it here without the values.
- The `known_hosts` change from a re-minted SSH host key has no automation; tell users.
- The k3s api ServiceAccount token (B.3) expires; nothing warns before it does.
- Credential rotation for everything except the region agent token is a procedure, not a
  script — `deploy/BACKUPS.md`, "Rotation". Workload identity would remove the storage key
  entirely; that migration is described there and not done.
