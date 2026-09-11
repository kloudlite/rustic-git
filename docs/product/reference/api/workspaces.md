# Workspaces API

## The document

```json
{
  "id": "ws-7f3a…",
  "owner": "karthik",
  "team": "",
  "name": "api",
  "region": "centralindia-k3s",
  "state": "ready",
  "image": "ghcr.io/kloudlite/kloudlite-workspace",
  "placement": "node-2",
  "volume": "vol-…",
  "quota_gb": 20,
  "ssh": { "gateway": "…", "host_key": "ssh-ed25519 AAAA…" },
  "packages": ["nodejs@22", "pnpm"],
  "base_packages": ["git", "openssh"],
  "packages_status": { "base": [], "observed": ["nodejs@22", "pnpm"], "observed_hash": "…", "profile": "/nix/store/…" },
  "replicated": { "ready": true, "reason": "Replicated", "message": "another node holds the final sync point" },
  "degraded": null,
  "decommissioning": null
}
```

`state` is one of `creating`, `ready`, `stopped`, `error`, `deleted`.

## `POST /workspaces`

| Field | Type | Required |
|---|---|---|
| `name` | string, 1–63 of `[A-Za-z0-9._-]` | yes |
| `region` | string | yes |
| `quota_gb` | integer | yes |
| `image` | string | no |
| `repo`, `branch` | string | no |
| `packages` | string[] | no |
| `team` | string | no |

Returns the document in `creating`. `400` bad field, `409` over quota, `422` unresolvable pin, `503` every index down.

## `GET /workspaces`

Your workspaces; `?team={slug}` for a team's.

## `GET /workspaces/{id}`

The document. `{id}` may be a name you own.

## `PATCH /workspaces/{id}`

`{"packages": [...]}`. Replaces the list; unchanged entries keep their locks.

## `POST /workspaces/{id}/packages/update`

Re-resolves every pinned entry. No body.

## `POST /workspaces/{id}/start` · `/stop`

No body. `409` on start when the workspace is interrupted.

## `DELETE /workspaces/{id}`

No body. Snapshots survive on a detached volume.

## `POST /workspaces/{id}/clone`

`{"name": "…"}`. Returns the new document plus `based_on: {snapshot, at, age_seconds, interrupted}`.

## `POST /workspaces/restore`

| Field | Required |
|---|---|
| `name` | yes |
| `snapshot_id` | yes |
| `image`, `packages`, `quota_gb`, `attached_environment` | no; default to the snapshot's frozen state |

## `POST /workspaces/{id}/push`

Optional `{"message": "…"}`. Returns the snapshot record.

## `POST /workspaces/{id}/attach` · `/detach`

Attach: `{"environment": "env-…"}`. Detach: no body.

## `POST /workspaces/{id}/ssh-session`

No body. Returns a connect ticket: a signed token, the gateway, and the host key. Used by `kl-connect`; nothing is stored.
