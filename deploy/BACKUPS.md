# Backups: what is protected, what is not, and the switches that make it so

Almost nothing here is a backup *job*. The product's data lives in Azure services whose own
retention features are the backup — and every one of them is off by default. This is the
checklist of which switches must be on, why, and what stays unprotected after all of them are.
Tick the boxes with the `az` output pasted underneath; a box without output is a claim.

## The stores

| Store | What is in it | Where | Copy count today |
| --- | --- | --- | --- |
| Blob container `kloudlite-git` (`KLOUDLITE_GIT_S3_URL: az://kloudlite-git`) | Every git repo, registry manifest and tag, PR row (SlateDB per repo/image/volume), credentials as plain keys, `index/` markers, registry blobs and manifests | storage account named in Secret `kloudlite-git-storage` | one (LRS unless changed) |
| Blob containers `wslayers`, `wslayers-k3s` (one per region) | Every pushed workspace/environment snapshot (btrfs send streams), content-addressed `blobs/{owner}/{algo}/{hex}` | one storage account per region; agent Secret `AZURE_*` | one per region |
| Cosmos DB, Mongo API (`kloudlite-git-mongo`) | Pull-request store used by the server tier | Cosmos account | Cosmos-managed periodic backup (default: 2 copies, 8 h interval, 8 h retention) |
| k3s SQLite `state.db` on `k3s-cp` | The CRDs: every Workspace, Environment, Region, Volume, Snapshot, VolumeReplica, OwnerBinding — the record of what the subvolumes and snapshots ARE, and (`Region`) what regions exist | one VM | hourly tarball to container `k3s-backup` (this repo's timer) |
| Blob container `k3s-backup` | 24 hourly + 7 daily slots of the above, fixed names that overwrite, AES-256 encrypted with `/etc/kloudlite-git/k3s-backup.key` on `k3s-cp` (a copy of that key must live in the password manager) | same account as `kloudlite-git` | one |
| Redis | `events` stream, caches, generation counters | managed instance | **none, deliberately** — a nudge and a view, never the record (CLAUDE.md) |
| btrfs pools on the pool nodes | Live workspace subvolumes | node data disks | **none** — the pushed snapshot is the backup; unpushed work on a lost node is gone |

## Checklist

### 1. Blob soft-delete and versioning on the SlateDB account — [ ]

Why both: SlateDB is log-structured. It rewrites its manifest and *deletes* old SSTs on
compaction, and the registry GC sweep and `DELETE /v2/.../blobs` delete for real. Soft-delete
keeps a deleted object recoverable; versioning keeps every overwritten manifest. Container
soft-delete is the guard against `az storage container delete kloudlite-git` with the key that
sits in six pod specs.

```sh
ACCT=<account from Secret kloudlite-git-storage>
az storage account blob-service-properties update --account-name "$ACCT" \
  --enable-delete-retention true --delete-retention-days 14 \
  --enable-container-delete-retention true --container-delete-retention-days 14 \
  --enable-versioning true
az storage account blob-service-properties show --account-name "$ACCT" \
  --query '{del:deleteRetentionPolicy,cont:containerDeleteRetentionPolicy,ver:isVersioningEnabled}'
```

Paste the `show` output here:

```
(pending)
```

Cost note: versioning on a SlateDB container keeps every compacted SST for 14 days. Expect the
container to grow by roughly one compaction's worth per day; watch it for a week before trusting
the number. Versions older than the window are removed only by a lifecycle rule:

```sh
az storage account management-policy create --account-name "$ACCT" --policy '{"rules":[{"name":"expire-versions","enabled":true,"type":"Lifecycle","definition":{"filters":{"blobTypes":["blockBlob"]},"actions":{"version":{"delete":{"daysAfterCreationGreaterThan":14}}}}}]}'
```

### 2. The same on every `wslayers*` account — [ ]

Snapshot blobs are content-addressed and never overwritten, so versioning buys nothing there;
soft-delete (blob + container, 14 d) is what matters. Same commands as above per region account
minus `--enable-versioning`. One box per region:

- [ ] `centralindia-vm` account: `(pending)`
- [ ] k3s region account (`wslayers-k3s`): `(pending)`

### 3. Cosmos continuous backup — [ ]

The default periodic policy keeps 8 hours. Continuous (7-day tier) gives point-in-time restore
to any second in the last week, which is the only way back from a bad write to the PR store.
It is a one-way migration per account and cannot be turned off.

```sh
az cosmosdb update -g <rg> -n kloudlite-git-mongo --backup-policy-type Continuous --continuous-tier Continuous7Days
az cosmosdb show -g <rg> -n kloudlite-git-mongo --query 'backupPolicy'
```

`Region` is a CRD now, not a Cosmos row — it is covered by the k3s SQLite tarball in the store
table above (a handful of objects, rebuildable by hand in minutes via `POST /v1/regions` even
without that backup). Paste output:

```
(pending)
```

### 4. Redundancy — [ ]

All of the above are recoveries *within* one region. `az storage account show --query sku`
reports the replication. `Standard_LRS` means one datacentre; a regional loss is a total loss.
Decide and record: `(pending — LRS/ZRS/GRS)`. Changing it is `az storage account update --sku`,
online, and the only cost is the price.

### 5. The k3s control plane — [ ]

`deploy/k3s/backup-controlplane.timer` (install steps in `deploy/k3s/README.md`). Verify:

```sh
ssh azureuser@<k3s-cp> 'systemctl list-timers backup-controlplane.timer; systemctl status backup-controlplane.service --no-pager | head -5'
az storage blob list --account-name "$ACCT" -c k3s-backup --query '[].{n:name,t:properties.lastModified}' -o table
```

The newest `hourly-*` blob must be under 2 hours old. Blob versioning on the `k3s-backup`
container (checklist 1 covers it — same account) is what turns the 24+7 fixed slots into a
history longer than a week, and what saves the good backup a bad one overwrote.

### 6. The snitch — [ ]

`SNITCH_URL=` in `/etc/kloudlite-git/k3s-backup.env` on `k3s-cp`, pointing at a healthchecks.io-style
monitor with a 1 h period and a grace of 30 min. This is the only alert on the whole page: every
other row is a *retention setting*, which fails silently by definition. Monitor URL recorded
where: `(pending)`.

## A restore drill, once

Before trusting any of this, restore one thing of each kind and write the date here.

- Blob: `az storage blob undelete` on a soft-deleted object under `kloudlite-git/`; read it back.
- Version: `az storage blob copy start --source-blob X --source-blob-version-id <id>`.
- Cosmos: `az cosmosdb sql database restore` to a new account (Mongo: `mongodb database restore`)
  at a timestamp 10 minutes ago; count documents.
- k3s: the procedure in the trailing comment of `deploy/k3s/backup-controlplane.sh`, onto a
  scratch VM — the `-wal`/`-shm` removal is the step that bites.

Last drill: `(never)`.

## What is NOT backed up, and why that is or is not acceptable

- **Unpushed workspace state.** The btrfs subvolume on a pool node has one copy. A node loss
  loses whatever was not `push`ed. Acceptable by design: push is cheap and the product says so;
  a scheduled auto-push would be the fix if that changes.
- **Redis.** Nothing in it is the record (the worker beats and the feed's `pulls_across`
  fallback are verified to work with Redis down). Loss = a slower minute.
- **SlateDB point-in-time consistency.** Blob versioning restores *objects*, not a *database*:
  a consistent SlateDB restore needs the manifest and every SST it references at one instant.
  Recovering one repo means finding its manifest version at time T and undeleting the SSTs it
  names — doable by hand, unpractised, and slow. There is no tested procedure; the drill above
  restores an object, not a repo.
- **Secrets.** The ten `kloudlite-git-*` Secrets on AKS and `kloudlite-git-agent` on k3s are created
  by hand and exist nowhere else (the k3s backup's `identity.tgz` covers the cluster CA and join
  token, not these). A from-scratch rebuild re-mints them — `deploy/RECOVERY.md` is that
  procedure, every Secret with its keys and where each value comes from; the value that cannot
  be re-minted (the storage account key) is recoverable from the Azure portal.
  Keep it that way rather than adding a backup that is itself a secret store.
- **Cross-region.** Every mechanism here is single-region. A region loss is a rebuild from the
  other region's `wslayers` plus whatever GRS was enabled in step 4.

## Credentials: who holds what, and the least each needs

Every Azure credential today is a long-lived ACCOUNT KEY in a hand-made Secret, and the same
key is handed to every tier (audit I-6). The storage key in particular is full read/write/delete
on every blob — the GC sweep's authority — and it sits in six pod specs, including the api tier,
which never opens a repository for writing. This is the table of what each tier actually does
with each store, which is the table any narrowing has to preserve:

| Secret | Tier | Needs | Holds today |
| --- | --- | --- | --- |
| `kloudlite-git-storage` | srv | read + write + delete on `kloudlite-git` (SlateDB compaction deletes SSTs; `DELETE /v2/.../blobs`) | account key |
| `kloudlite-git-storage` | worker | read + write + delete (the GC sweep, marker reconcile) | account key |
| `kloudlite-git-storage` | api | read (browse, `/api/{owner}/images`, `_catalog`), write of `auth/`/`index/` keys | account key |
| `kloudlite-git-agent` `AZURE_*` (historical — the agent no longer holds Azure credentials at all; see Task 8) | k3s agent | read + write on the region's `wslayers*` (push uploads, restore reads); never delete | account key |
| `kloudlite-git-mongo` | srv, api, worker | read + write (directory, PRs) | connection string |
| `kloudlite-git-jwt`, `kloudlite-git-peer` | as `deploy/RECOVERY.md` A.2 | symmetric — the same value everywhere by design | minted value |

**Per-tier Secrets with the minimum role each**, the half that needs no code change and is
the state this file asks for: `kloudlite-git-storage` stays the key-holding Secret for srv
and worker; the api tier gets its own `kloudlite-git-storage-api`, and the agent's `AZURE_KEY`
becomes a container-scoped SAS (`racw` on `wslayers-k3s`, one-year expiry, minted by
`az storage container generate-sas`) so a leaked agent Secret cannot read `kloudlite-git` or
delete anything.
None of this needs the binaries to change: the storage crate authenticates with whatever
`AZURE_STORAGE_ACCOUNT_KEY`/SAS it is given. Blocked on: the
api tier's `auth/` and `index/` writes, which need a `w` in its scope, so "reader" is not
literally true for it — the split it gets is *no delete*, not read-only.

**The end state is AKS workload identity** — no key exists to leak, rotate, or back up:
Microsoft Entra Workload ID on the cluster, a user-assigned managed identity per tier (three:
server+worker, api, and the k3s agents via a federated credential on their ServiceAccount),
role assignments `Storage Blob Data Contributor` (server/worker), `Storage Blob Data Reader` +
a scoped exception for `auth/`/`index/` writes (api), `Storage Blob Data Contributor` on the
region account only (agent), and `Cosmos DB Built-in Data Reader`/`Contributor` for the
directory. For blob storage there may be NO code change: `crates/storage/src/config.rs`
builds its client with `MicrosoftAzureBuilder::from_env()`, and object_store 0.14 reads
`AZURE_FEDERATED_TOKEN_FILE` + `AZURE_CLIENT_ID` + `AZURE_TENANT_ID` through that same path
(`azure/builder.rs`, `AzureConfigKey::FederatedTokenFile`) — exactly the three variables the
workload-identity webhook injects when the pod's ServiceAccount carries the
`azure.workload.identity/client-id` annotation. Dropping the `AZURE_STORAGE_ACCOUNT_KEY` env
from the specs is the switch. Verify line when it is: `kubectl -n kloudlite-git get secret
kloudlite-git-storage` returns NotFound and the fleet still serves.

## Rotation

Every credential rotates by the procedure below; rotating after an incident is the point, so each
one is written to be run under pressure. Every symmetric secret is a two-cluster or two-Secret change, and the
outage window is the gap between the two halves — do them in one sitting.

| Credential | Procedure | Outage |
| --- | --- | --- |
| Storage account key | `az storage account keys renew -n <acct> --key key2` → patch the Secret(s) with key2 → `deploy/roll.sh` → renew key1 once every pod is on key2. Two keys exist precisely so rotation is never a gap | none, if key2 is rolled before key1 is renewed |
| `wslayers*` key or SAS (agent) — historical, the agent holds no Azure credential any more | same two-key dance on the region account; patch `kloudlite-git-agent` `AZURE_KEY`; `kubectl -n kube-system rollout restart ds/kloudlite-git-agent`. Only this region's own agents hold the key, so there is no second cluster to patch | in-flight pushes retry |
| Cosmos key (`kloudlite-git-mongo`) | `az cosmosdb keys regenerate --key-kind secondary` → patch `kloudlite-git-mongo` to the secondary → roll → regenerate primary | none, same reason |
| `kloudlite-git-jwt` **(two clusters)** | new value → `kubectl -n kloudlite-git patch secret kloudlite-git-jwt` on AKS AND `kubectl -n kloudlite-git-system patch secret kloudlite-git-jwt` on k3s → `deploy/roll.sh` → `kubectl -n kloudlite-git-system rollout restart deploy/kloudlite-git-gateway`. Every signed-in session and every `docker login` bearer is invalidated: users sign in again, clients `docker login` again | every session, once |
| `kloudlite-git-peer` | new value → patch → `deploy/roll.sh`. During the roll, old and new pods cannot forward to each other: 421s until the last pod is on the new value | minutes of misdirected writes |
| SSH host key | do not, unless compromised: every user's `known_hosts` breaks. If forced: `deploy/RECOVERY.md` A.2, then announce the new fingerprint | every SSH user, once |
| k3s api ServiceAccount token | `kubectl -n kube-system create token kloudlite-git-api --duration=8760h` on k3s → rebuild the kubeconfig (`deploy/RECOVERY.md` B.3) → patch `kloudlite-git-k3s-kubeconfig` → `kubectl -n kloudlite-git rollout restart deploy/kloudlite-git-api`. It EXPIRES — put the date somewhere that pages | none |
| k3s backup key | only if compromised: new key in `/etc/kloudlite-git/k3s-backup.key` AND the vault, keep the old one in the vault too (older bundles need it) | none |
| Cloudflare Origin CA cert (`gateway-tls`, optional) | 15-year cert; re-issue in the dashboard, `kubectl -n kloudlite-git-system create secret tls gateway-tls ... --dry-run=client -o yaml \| kubectl apply -f -`, restart the gateway | reconnects |

Last rotation of each, with the date, belongs in this table's margin: `(never)`.
