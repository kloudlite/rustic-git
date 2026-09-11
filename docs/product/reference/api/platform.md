# Platform API

## `GET /regions`

```json
[{ "id": "centralindia-k3s", "name": "Central India", "status": "active" }]
```

## `GET /quota`

```json
{ "owner": "karthik", "limit": { "workspaces": 5, "environments": 2, "snapshots": 20, "disk_gb": 100, "cpu": 40, "memory_gb": 80 }, "used": { … } }
```

`?owner={team}` for a team you belong to.

## `GET /builders/me`

`?team={slug}` for a team's. `{ "id", "state", "ready", "conditions" }`. Never listed elsewhere.

## Requests

### `POST /requests`

| Field | Required |
|---|---|
| `kind` | yes: `quota`, `access`, `region`, `other` |
| `reason` | yes |
| `owner` | no; a team slug you administer |
| `quota` | for `quota`: `{workspaces?, environments?, snapshots?, disk_gb?, cpu?, memory_gb?}` |
| `access` | for `access`: `{team, role}` |
| `region` | for `region`: `{region, title, body}` |
| `other` | for `other`: `{title, body}` |

`409` when a pending request of that kind already exists for the owner.

### `GET /requests` · `GET /requests/{id}`

`{ id, owner, kind, requested_by, reason, quota, access, region, other, state, … }`.

## Keys

| Method | Path | Body |
|---|---|---|
| `POST` | `/keys` | `{ "name", "key", "signing": false }` — `owner` in the body is `400` |
| `GET` | `/keys` | |
| `DELETE` | `/keys/{id}` | |

## Tokens

| Method | Path | Body |
|---|---|---|
| `POST` | `/tokens` | `{ "name" }` (session auth) |
| `GET` | `/tokens` | |
| `DELETE` | `/tokens/{id}` | |
| `GET` | `/cli/tokens` | Tokens minted by `kl-connect login` |
| `DELETE` | `/cli/tokens/{id}` | |

## Teams

| Method | Path | Body |
|---|---|---|
| `POST` | `/teams` | `{ "slug", "name" }` |
| `GET` | `/teams` | |
| `GET` | `/teams/{slug}/profile` | |
| `POST` | `/teams/{slug}/invites` | `{ "email", "role": "member" \| "admin" }` |
| `DELETE` | `/teams/{slug}/invites/{id}` | |
