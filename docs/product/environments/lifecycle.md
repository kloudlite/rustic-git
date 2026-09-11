# Lifecycle

Start, stop, and delete for an environment mirror a workspace's, with one difference: a stop drains every service before it cuts.

## Stop

Cuts a sync point of the disk, then removes every StatefulSet. State becomes `stopped`. Data is kept.

```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/environments/$ENV/stop -H "Authorization: Bearer $KL_TOKEN"
```

## Start

Applies every service again on the node that holds the newest sync point.

```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/environments/$ENV/start -H "Authorization: Bearer $KL_TOKEN"
```

An environment running on a node that is down cannot be started elsewhere and cannot be cloned; it resumes when the node returns. A stopped one starts on any up-to-date node.

## Delete

Removes the services and the disk. Pushed snapshots survive on a detached [volume](../snapshots/volumes.md).

```bash [API]
curl -sS -X DELETE https://dev.kloudlite.io/v1/environments/$ENV -H "Authorization: Bearer $KL_TOKEN"
```

Any intercept wishes on the environment go with it.

## Restore in place

Swap the disk under the same environment for a snapshot's bytes. Services drain, the disk is replaced, services come back. `restoring` on the document reads `Draining` then `Restoring` while it runs, and `restored_to` names the snapshot afterwards.

```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/environments/$ENV/restore-in-place \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{"snapshot_id": "push-env-2b8c-77aa"}'
```
