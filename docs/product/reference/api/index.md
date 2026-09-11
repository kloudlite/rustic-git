# API overview

Base URL `https://dev.kloudlite.io/v1`. Every request carries `Authorization: Bearer <token>`; every body and response is JSON. The console and both CLIs use these routes and nothing else.

## Conventions

| | |
|---|---|
| Ids | Opaque strings: `ws-…`, `env-…`, `push-…`. Workspace routes also accept a name you own |
| Owner | Derived from the token, or named with `team` / `owner` on create for a team you belong to |
| Errors | A JSON body `{"error": "…"}` with one sentence |
| `404` | Also the answer for an object you may not act on; nothing distinguishes the two |
| `409` | A state conflict, or over quota (the sentence names the dimension) |
| `422` | A package pin that cannot be resolved or is not cached |

## Routes

### Workspaces — [reference](workspaces.md)

| Method | Path |
|---|---|
| `POST` | `/workspaces` |
| `GET` | `/workspaces` |
| `POST` | `/workspaces/restore` |
| `GET` `PATCH` `DELETE` | `/workspaces/{id}` |
| `POST` | `/workspaces/{id}/packages/update` |
| `POST` | `/workspaces/{id}/clone` · `/push` · `/start` · `/stop` · `/attach` · `/detach` · `/ssh-session` |

### Environments — [reference](environments.md)

| Method | Path |
|---|---|
| `POST` | `/environments` |
| `GET` | `/environments` |
| `POST` | `/environments/restore` |
| `GET` `DELETE` | `/environments/{id}` |
| `POST` | `/environments/{id}/clone` · `/push` · `/start` · `/stop` · `/restore-in-place` |
| `POST` | `/environments/{id}/intercepts` |
| `DELETE` | `/environments/{id}/intercepts/{service}` |

### Snapshots and volumes — [reference](snapshots.md)

| Method | Path |
|---|---|
| `GET` | `/volumes` |
| `GET` | `/volumes/{name}/history` · `/volumes/{name}/refs` |
| `DELETE` | `/volumes/{name}` · `/volumes/{name}/snapshots/{snapshot}` |

### Platform — [reference](platform.md)

| Method | Path |
|---|---|
| `GET` | `/regions` · `/quota` · `/builders/me` |
| `POST` `GET` | `/requests` · `/requests/{id}` |
| `POST` `GET` `DELETE` | `/keys` · `/keys/{id}` · `/tokens` · `/tokens/{id}` · `/cli/tokens` · `/cli/tokens/{id}` |
| `POST` `GET` | `/teams` · `/teams/{slug}/profile` · `/teams/{slug}/invites` · `/teams/{slug}/invites/{id}` |
