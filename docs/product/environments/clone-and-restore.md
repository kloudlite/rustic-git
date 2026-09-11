# Clone and restore

`clone` copies an environment's live disk into a new environment. `restore` creates one from a pushed snapshot, with the frozen services unless you say otherwise.

## Clone

Copies bytes from the source's live subvolume on the node that holds it, so it works on a running or stopped environment but not an interrupted one (`409`). No sync point is cut and there is no `based_on`.

```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/environments/$ENV/clone \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{"name": "acme-dev-2"}'
```

## Restore

```bash [API]
curl -sS https://dev.kloudlite.io/v1/environments/restore \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{
    "name": "acme-dev-yesterday",
    "snapshot_id": "push-env-2b8c-77aa",
    "owner": "acme"
  }'
```

| Field | Default |
|---|---|
| `name` | required |
| `snapshot_id` | required |
| `owner` | you |
| `services` | the services the snapshot froze; an empty list means the same. A snapshot that froze none needs them here |
| `quota_gb` | the snapshot's |

## Restore in place

To bring a snapshot back under the same environment and the same DNS names, use [restore in place](lifecycle.md#restore-in-place).
