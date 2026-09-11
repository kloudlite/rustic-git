# Snapshots and volumes API

## `GET /volumes`

Every volume you may act on.

```json
[{ "name": "ws-7f3a…", "kind": "workspace", "volume": "vol-…", "display_name": "api", "deleted": false, "latest_ms": 1757580131000, "snapshots": 4, "last_push_at": "2026-09-10T17:02:11Z" }]
```

| Field | Meaning |
|---|---|
| `name` | The working copy's id, kept as the volume's handle after deletion |
| `kind` | `workspace` or `environment` |
| `deleted` | `true` for a detached volume |
| `latest_ms` | Approximate time of the last write |
| `snapshots` | Pushed snapshots on it |

## `GET /volumes/{name}/history`

Pushed snapshots, newest first.

```json
[{ "id": "push-ws-7f3a-4e5f", "parent": "push-ws-7f3a-11aa", "phase": "Ready", "message": "…", "state": { … } }]
```

`phase` is `Pending` until cut and replicated, then `Ready`. `state` is the frozen definition. `parent` is `null` for the first snapshot.

## `GET /volumes/{name}/refs`

`{"main": "<newest snapshot id>"}`; `{"main": null}` for a volume with no pushes. Never `404`.

## `DELETE /volumes/{name}/snapshots/{snapshot}`

`409` for the base of a running working copy. Deleting a detached volume's last snapshot deletes the volume.

## `DELETE /volumes/{name}`

A detached volume with all its snapshots. `409` while a working copy exists.
