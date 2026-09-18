# Backup and restore coverage

This inventory describes the repository architecture reviewed on 18 September 2026. Cloud retention settings, live replica counts, backup jobs, and restore success were not verified during that review. Record current evidence before treating any store as recoverable.

## Authoritative stores

| Store | Current contents | Recovery evidence required |
| --- | --- | --- |
| Configured object store (`KLOUDLITE_S3_URL`) | Git packs and per-repository/image SlateDB state, pull requests, authentication keys, listing markers, OCI manifests and layers | Backend retention/versioning evidence and a consistent repository/image restore drill |
| Registry generation records and bytes | `blob-state/`, `blob-generations/`, plus adopted legacy `blobs/` objects | Restore state, active physical generations, manifests, and image metadata together; follow the [generation migration/recovery procedure](../docs/registry-generation-rollout.md) |
| Directory database (Mongo API) | People, teams, memberships, credentials and related directory records | Current backup policy and an isolated directory restore with identity/membership checks |
| Kubernetes control planes | Desired/observed workspace state, including Workspace, Environment, Volume, Snapshot and VolumeReplica records; other CRDs and resources depend on the deployed manifests | A control-plane restore and reconciliation against the matching storage state |
| Node btrfs pools | Live workspace/environment/bench worktrees, explicit snapshots, automatic sync points and local replicas | Actual ready replica locations, their generation/freshness, and an isolated node-loss/rehost/restore exercise |
| Regional NFS homes | Shared home configuration and files selected by the workload mounts | The export's actual storage/backup policy and a separate file restore |
| ClickHouse / ClickStack | Product history, SLO results and telemetry | Current retention and backup configuration, plus an isolated queryable restore |
| History outbox, when enabled by the hardening release | Pending lifecycle event batches under `history-outbox/` in object storage | Pending original rows survive restart and reach ClickHouse before acknowledgment/removal |
| Redis | Event stream, cache and notification state | An explicit accepted loss policy; critical controllers/workers retain independent reconciliation paths |

`index/` objects are listing views, never authorization. They can be rebuilt from authoritative state; credentials and database contents cannot be reconstructed from those markers.

## Workspace durability

Current workspace snapshots use btrfs subvolumes and node-to-node replication. They do not use Azure `wslayers` containers. The old object-store snapshot architecture and its agent `AZURE_*` credential instructions are historical and are not recovery steps for the current engine.

An explicit push and an automatic sync point are distinct records. Durability depends on the replicas that actually reached Ready and the generation they hold. A node failure can lose writes newer than the latest surviving copy. Neither a Snapshot CR alone nor a configured replica target proves that its bytes exist elsewhere. Replication within a region also does not establish independent backup or protection against a region-wide loss.

Inspect workload mounts when determining where bench transcripts and home files live. Back up the current pool/export paths; do not infer their location from a retired pod layout.

## Evidence checklist

- [ ] Record backend/account/container identifiers from deployment configuration without copying credential values.
- [ ] Record current object deletion retention, versioning, lifecycle rules and redundancy. Confirm that a lifecycle rule cannot remove generations or pending outbox entries outside their application protocols.
- [ ] Record the directory database's actual backup policy, retention and successful restore target.
- [ ] Verify the configured k3s control-plane backup timer, newest successful artifact, encryption-key custody and alerting. Source configuration is in `deploy/k3s/backup-controlplane.sh` and its timer; installation instructions are in `deploy/k3s/README.md`.
- [ ] Record each volume's surviving snapshot/sync-point replicas and last complete synchronization. Exercise recovery using isolated resources.
- [ ] Record the NFS export and ClickHouse storage policies separately; neither is covered merely by retaining OCI/Git objects.
- [ ] Maintain the secret/configuration recovery inventory in [RECOVERY.md](RECOVERY.md), checking its instructions against the current manifests before use. Store recovery material in the approved secret store, not this document.
- [ ] Record the last successful drill, source/image versions, recovery point, recovery duration and any missing data. Until populated, these are unverified requirements.

## Restore drills

Use isolated destinations and a recorded source checkpoint. An object undelete is not a database restore.

1. Restore one repository's consistent SlateDB manifest and referenced files; clone it and verify known commits and refs.
2. Restore an OCI image's database, verbatim manifests, generation records and referenced physical blobs. Pull it through the registry and verify digests before enabling collection.
3. Restore directory records to an isolated database and verify expected identity and membership behavior.
4. Restore Kubernetes state to an isolated control plane, then reconcile a selected workspace against its surviving btrfs snapshot/sync-point bytes. Verify expected files and replica placement.
5. Restore selected NFS home files and query restored history. If an outbox is included, preserve original event timestamps and IDs when replaying it.

Last evidenced drill: **not recorded here**. Recovery-point and recovery-time objectives: **not yet validated**.

## Known limits and credential scope

Redis loss can remove stream-only PR activity-feed evidence even when worker reconciliation continues. Do not describe the entire feed as recoverable from a fallback. A history outbox protects batches after durable enqueue; it does not reconstruct transitions never captured before a process crash or Kubernetes watch compaction.

Object versioning provides individual historical objects. A consistent SlateDB or registry recovery also needs every referenced object at the chosen checkpoint. Workspace recovery needs matching CRDs and surviving pool/export bytes. Regional replication and backup must be evaluated as separate properties.

Review object-store permissions from actual callers before narrowing them: server compaction/registry operations delete objects; workers collect retired generations and reconcile markers; API/admin code writes authentication/settings/audit data and, when enabled, manages history outbox objects. The current node agent does not need historical workspace-snapshot object-store credentials. Do not provision those credentials from an old runbook.

Credential rotation and workload-identity changes require their own reviewed provider-specific procedure and verification. The earlier Azure-specific proposals in this file were not evidence of deployed permissions or tested rotation. Check current manifests and provider documentation before making those changes.
