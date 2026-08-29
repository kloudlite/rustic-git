# Backups: what is protected, what is not, and the switches that make it so

Almost nothing here is a backup *job*. The product's data lives in Azure services whose own
retention features are the backup — and every one of them is off by default. This is the
checklist of which switches must be on, why, and what stays unprotected after all of them are.
Tick the boxes with the `az` output pasted underneath; a box without output is a claim.

## The stores

| Store | What is in it | Where | Copy count today |
| --- | --- | --- | --- |
| Blob container `rustic-git` (`RUSTIC_GIT_S3_URL: az://rustic-git`) | Every git repo, registry manifest and tag, PR row (SlateDB per repo/image/volume), credentials as plain keys, `index/` markers, registry blobs and manifests | storage account named in Secret `rustic-git-storage` | one (LRS unless changed) |
| Blob containers `wslayers`, `wslayers-k3s` (one per region) | Every pushed workspace/environment snapshot (btrfs send streams), content-addressed `blobs/{owner}/{algo}/{hex}` | one storage account per region; agent Secret `AZURE_*` | one per region |
| Cosmos DB, Mongo API (`rustic-git-mongo`) | Pull-request store used by the server tier | Cosmos account | Cosmos-managed periodic backup (default: 2 copies, 8 h interval, 8 h retention) |
| Cosmos DB, SQL/Core (`rustic-git-cosmos`, db `workspaces`) | `Region` metadata only — nothing else lives here any more; the CRD wins on any disagreement | Cosmos account | same default |
| k3s SQLite `state.db` on `k3s-cp` | The CRDs: every Workspace, Environment, Volume, OwnerBinding, SnapshotRequest — the record of what the subvolumes and snapshots ARE | one VM | hourly tarball to container `k3s-backup` (this repo's timer) |
| Blob container `k3s-backup` | 24 hourly + 7 daily slots of the above, fixed names that overwrite | same account as `rustic-git` | one |
| Redis | `events` stream, caches, generation counters | managed instance | **none, deliberately** — a nudge and a view, never the record (CLAUDE.md) |
| btrfs pools on the pool nodes | Live workspace subvolumes | node data disks | **none** — the pushed snapshot is the backup; unpushed work on a lost node is gone |

## Checklist

### 1. Blob soft-delete and versioning on the SlateDB account — [ ]

Why both: SlateDB is log-structured. It rewrites its manifest and *deletes* old SSTs on
compaction, and the registry GC sweep and `DELETE /v2/.../blobs` delete for real. Soft-delete
keeps a deleted object recoverable; versioning keeps every overwritten manifest. Container
soft-delete is the guard against `az storage container delete rustic-git` with the key that
sits in six pod specs.

```sh
ACCT=<account from Secret rustic-git-storage>
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
for A in rustic-git-mongo rustic-git-cosmos; do
  az cosmosdb update -g <rg> -n "$A" --backup-policy-type Continuous --continuous-tier Continuous7Days
  az cosmosdb show -g <rg> -n "$A" --query 'backupPolicy'
done
```

`rustic-git-cosmos` holds only `Region` rows (a handful, rebuilt by hand in minutes via
`/v1/regions`); it is on this list for uniformity, not necessity. Paste output:

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

`SNITCH_URL=` in `/etc/rustic-git/k3s-backup.env` on `k3s-cp`, pointing at a healthchecks.io-style
monitor with a 1 h period and a grace of 30 min. This is the only alert on the whole page: every
other row is a *retention setting*, which fails silently by definition. Monitor URL recorded
where: `(pending)`.

## A restore drill, once

Before trusting any of this, restore one thing of each kind and write the date here.

- Blob: `az storage blob undelete` on a soft-deleted object under `rustic-git/`; read it back.
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
- **Secrets.** The ten `rustic-git-*` Secrets on AKS and `rustic-git-agent` on k3s are created
  by hand and exist nowhere else (the k3s backup's `identity.tgz` covers the cluster CA and join
  token, not these). A from-scratch rebuild re-mints them; the values that cannot be re-minted
  (the storage account key, Cosmos keys) are recoverable from the Azure portal. Keep it that way
  rather than adding a backup that is itself a secret store.
- **Cross-region.** Every mechanism here is single-region. A region loss is a rebuild from the
  other region's `wslayers` plus whatever GRS was enabled in step 4.
