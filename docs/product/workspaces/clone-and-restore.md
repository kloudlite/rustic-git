# Clone and restore

Two ways to get a new workspace from an existing tree. `clone` copies a workspace as it is now. `restore` starts from a snapshot you pushed earlier.

## Clone

Cuts a fresh sync point of the source at the moment of the request and creates a new workspace on it, with the source's image, packages, and quota.

```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/workspaces/$WS/clone \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{"name": "api-2"}'
```

The response is the new workspace plus `based_on`:

```json
{
  "id": "ws-9c1d…",
  "name": "api-2",
  "state": "creating",
  "based_on": { "snapshot": "clone-ws-7f3a-1a2b", "at": "2026-09-11T08:12:04Z", "age_seconds": 0, "interrupted": false }
}
```

When the source is interrupted (its node is down) it cannot be cut, so the clone grafts onto the newest sync point another node holds. `interrupted: true` and `age_seconds` say how old that is.

## Restore

Creates a workspace from a snapshot by id. The snapshot froze the source's definition; every field in the body overrides one, and an absent field means the snapshot's value.

```bash [API]
curl -sS https://dev.kloudlite.io/v1/workspaces/restore \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{
    "name": "api-last-week",
    "snapshot_id": "push-ws-7f3a-4e5f",
    "packages": ["nodejs@22", "pnpm"],
    "quota_gb": 30
  }'
```

| Field | Default |
|---|---|
| `name` | required |
| `snapshot_id` | required; a pushed snapshot, from [history](../snapshots/history.md) |
| `image`, `packages`, `quota_gb`, `attached_environment` | what the snapshot froze |

A restore re-attaches the snapshot's volume, so it works after the source workspace was deleted.

## Which one

| You want | Use |
|---|---|
| A second copy of what is on disk now | clone |
| A copy of a specific pushed point | restore |
| A workspace back after its node died | clone (`based_on` tells you the age) |
| A workspace back after you deleted it | restore |
