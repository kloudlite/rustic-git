# Registry generations: migration and recovery

Status: proposed maintenance procedure for the September 2026 hardening work. These commands have not been run. The implementation and its carrying commit must pass review and validation before an operator approves this procedure. See [the implementation tracker](review-hardening-2026-09-18.md).

## Storage contract

`blob-state/{owner}/{algorithm}/{digest}` is the CAS record for a digest. It selects one active physical generation, records publication pins, and retains generations awaiting deletion. New bytes live under `blob-generations/{owner}/{algorithm}/{digest}/{generation}`. Existing `blobs/{owner}/{algorithm}/{digest}` objects may become legacy generations. Manifest bytes and image databases keep their existing locations.

Every state transition changes its nonce. Publication pins the active generation before checking bytes and writing the manifest. GC captures versions before its reference scan, retires only by CAS against those versions, then deletes immutable retired keys. Delayed deletion cannot remove a replacement generation.

Generation-aware readers must respect an existing record with no active generation; falling back to a legacy physical object would resurrect a retired blob. Never remove state records to force legacy fallback. Restore state records, referenced physical generations, manifests, and image metadata as a consistent set.

## Prerequisites

- Record the exact full source SHA, image digests, backend, current replica counts, and successful dev-pod checks. Include legacy adoption, real publication/GC interleavings, delayed deletion after reupload, CAS ABA, upload modes, sha512, shared layers, and failure injection. A `file://` installation also needs a cross-process local CAS test.
- Verify that the selected artifacts include every R04 follow-up. A published image tag alone does not establish this.
- Prepare a maintenance window for Git and OCI service interruption. This initial migration is not an ordinary rolling upgrade: old readers cannot resolve new generations, and old collectors/writers do not participate in the protocol.
- Inventory every process using this object store, including other clusters, development instances, temporary workers, jobs, and administrative commands. The commands below cover only the named deployment in `deploy/kloudlite.yaml`.
- Hold the deployment/probe coordination guard for the entire operation, suspend probe schedules while preserving their original values, and drain admitted probes. `--wait-only` alone does not reserve an interval.
- Pause autoscaling and reconciliation that could recreate old server/worker pods or restore their previous templates. Save reviewed deployment settings in the release record. Keep credentials out of that record.
- Establish an isolated restore point and validate its consistency before mutation. Cloud object retention by itself is not a tested registry restore.

## Maintenance sequence

Use a reviewed kube context and replacement image digest. Values below intentionally require an operator's release record. Do not run a broad manifest apply during the quiesced interval: it can restore replicas and restart an old collector.

```sh
export REGISTRY_CONTEXT='REPLACE_WITH_REVIEWED_CONTEXT'
export REGISTRY_NAMESPACE='kloudlite'
export REGISTRY_RELEASE_IMAGE='REPLACE_WITH_IMAGE_AT_SHA256_DIGEST'
export REGISTRY_RECORD_DIR='REPLACE_WITH_RELEASE_RECORD_DIRECTORY'

kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" get \
  statefulset/kloudlite-srv deployment/kloudlite-worker -o json \
  > "$REGISTRY_RECORD_DIR/workloads-before.json"
```

1. **Stop old collectors first.** Block new registry writes at every admission path, including peer/internal and development access. Scale the worker down, and wait for its pods to terminate normally. Verify separately that every external collector from the inventory is stopped.

```sh
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" \
  scale deployment/kloudlite-worker --replicas=0
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" \
  wait --for=delete pod -l app=kloudlite-worker --timeout=600s
```

2. **Quiesce all old writers/readers.** Scale the server StatefulSet to zero and wait for termination. A timeout stops the migration; do not force-delete a pod on an unreachable node and assume its process stopped. Resolve the node/process state and any outstanding object-store requests before continuing.

```sh
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" \
  scale statefulset/kloudlite-srv --replicas=0
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" \
  wait --for=delete pod -l app=kloudlite,role=server --timeout=900s
```

Pod absence alone does not prove that an already accepted remote DELETE has completed. Close this prerequisite using the backend's request/audit evidence and the old clients' request/retry bounds; record how outstanding operations were ruled out. If they cannot be ruled out, retain the maintenance state and investigate.

3. **Install compatible templates while both workloads remain stopped.** Verify the replacement container names against the saved objects. Persist matching image pins in the release manifests so later reconciliation cannot revert the templates.

```sh
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" set image \
  statefulset/kloudlite-srv "kloudlite=$REGISTRY_RELEASE_IMAGE"
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" set image \
  deployment/kloudlite-worker "worker=$REGISTRY_RELEASE_IMAGE"
```

4. **Start compatible servers first.** Restore the server replica count from the reviewed record. Keep collectors stopped and external writes gated while checking each server's actual image digest and readiness.

```sh
export REGISTRY_SERVER_REPLICAS='REPLACE_WITH_RECORDED_SERVER_COUNT'
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" \
  scale statefulset/kloudlite-srv --replicas="$REGISTRY_SERVER_REPLICAS"
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" \
  rollout status statefulset/kloudlite-srv --timeout=900s
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" get pods \
  -l app=kloudlite,role=server -o json
```

Verify a known legacy image pull, then an explicitly designated disposable repository's upload, manifest publication, pull, same-owner mount, and reupload. Record digest verification and physical/state consistency. These are future authorized maintenance checks, not tasks performed by this document.

5. **Start the compatible collector last.** Confirm no old collector can restart. Restore the recorded worker count, verify its image digest, and observe keep-biased collection and publication behavior. Reopen write admission and restore only the schedules and reconciliation settings recorded before maintenance. Release the coordination guard after checks and cleanup complete.

```sh
export REGISTRY_WORKER_REPLICAS='REPLACE_WITH_RECORDED_WORKER_COUNT'
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" \
  scale deployment/kloudlite-worker --replicas="$REGISTRY_WORKER_REPLICAS"
kubectl --context "$REGISTRY_CONTEXT" -n "$REGISTRY_NAMESPACE" \
  rollout status deployment/kloudlite-worker --timeout=600s
```

## Rollback boundary

Before compatible servers start, returning to the recorded old templates is possible only after confirming no migration state was written. After they start, even legacy reads may create state records; uploads can make generation-only bytes authoritative. Do not repin old binaries against that store. Prefer a compatible forward fix. An older binary requires a separately rehearsed reverse migration or a consistent pre-migration restore, with explicit handling of every write since that restore point.

Do not copy a few active objects back into canonical paths while collectors run. That loses retirement and pin history and allows an old delayed delete to target newly restored bytes.

## Recovery decisions

| Condition | Safe next step |
|---|---|
| Retired generation deletion failed | Retry deletion of that exact recorded immutable key through the protocol; remove its retirement entry only after confirmed deletion, using fresh CAS. |
| Manifest PUT or pin removal had an ambiguous result | Retain the pin. Release only after exact manifest identity/bytes and its reference to the pinned digest are established, using a supported conditional recovery operation. If the pin lacks that identity, retain it pending investigation. |
| Generation exists without known installation outcome | Keep it. A failed response does not prove CAS failed; it may be active or associated with unfinished publication. No age-only purge. |
| State is malformed, unavailable, or unexpectedly missing | Stop collection for the affected scope, preserve objects and diagnostic metadata, and investigate a consistent repair. Do not fabricate an empty state record. |
| Pin appears old or publisher process is gone | Age and process death alone do not establish publication outcome. Keep protection until durable evidence permits recovery. |
| Active generation is unreadable | Verify backend health and restore the recorded generation from a consistent recovery source. Do not redirect the digest to unverified bytes. |

No generic force-unpin, state-delete, or orphan-purge command is provided. A recovery helper must validate owner/digest/path identity, preserve unrelated pins and retired entries, use CAS, and treat ambiguous outcomes as retained state. Its dry-run evidence belongs in the release/recovery record.
