# History

Snapshots are read by volume, not by workspace, because a snapshot outlives the working copy it was taken from.

## Find the volume

`GET /v1/volumes` lists every volume you may act on: attached to a working copy or detached with snapshots only.

```bash [API]
curl -sS https://dev.kloudlite.io/v1/volumes -H "Authorization: Bearer $KL_TOKEN"
```

```json
[
  { "name": "ws-7f3a…", "kind": "workspace", "volume": "vol-…", "display_name": "api", "deleted": false, "snapshots": 4, "last_push_at": "2026-09-10T17:02:11Z" },
  { "name": "env-2b8c…", "kind": "environment", "volume": "vol-…", "display_name": "acme-dev", "deleted": true, "snapshots": 2, "last_push_at": "2026-09-08T09:40:00Z" }
]
```

`deleted: true` is a detached volume: its working copy is gone and only the snapshots remain.

## Walk the chain

```bash [API]
curl -sS https://dev.kloudlite.io/v1/volumes/$VOL/history -H "Authorization: Bearer $KL_TOKEN"
```

One row per pushed snapshot, newest first:

```json
[
  { "id": "push-ws-7f3a-4e5f", "parent": "push-ws-7f3a-11aa", "phase": "Ready", "message": "before the schema migration", "state": { "packages": ["nodejs@22", "pnpm"], "quota_gb": 20 } }
]
```

`state` is the frozen definition a restore defaults to.

## Refs

```bash [API]
curl -sS https://dev.kloudlite.io/v1/volumes/$VOL/refs -H "Authorization: Bearer $KL_TOKEN"
# {"main": "push-ws-7f3a-4e5f"}
```

There is one ref per volume, `main`, and it always names the newest pushed snapshot. A volume with no pushes answers `{"main": null}`.

## Restore

Take an id from history to [restore a workspace](../workspaces/clone-and-restore.md#restore) or [an environment](../environments/clone-and-restore.md#restore).

## Delete a snapshot

```bash [API]
curl -sS -X DELETE https://dev.kloudlite.io/v1/volumes/$VOL/snapshots/$SNAP -H "Authorization: Bearer $KL_TOKEN"
```

Refused with `409` for the base of a running working copy. Deleting a detached volume's last snapshot deletes the volume.
