# Create a workspace

`POST /v1/workspaces` writes one object and returns it in `creating`. The platform places it, builds its package profile, and reports `ready` with an ssh endpoint. Nothing is stored about it until the response.

## Request

::: tabs
```bash [API]
curl -sS https://dev.kloudlite.io/v1/workspaces \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{
    "name": "api",
    "region": "centralindia-k3s",
    "quota_gb": 20,
    "image": "ghcr.io/kloudlite/kloudlite-workspace",
    "repo": "git@github.com:acme/api.git",
    "branch": "main",
    "packages": ["nodejs@22", "pnpm", "postgresql_16"]
  }'
```
```text [Console]
Workspaces → New workspace
Name, region, disk, packages, and an optional repository to clone.
```
:::

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | 1–63 characters of letters, digits, `.`, `_`, `-`. Unique per owner; it is also the tree's directory name under `/home/kl/workspaces` |
| `region` | yes | A region id from `GET /v1/regions` |
| `quota_gb` | yes | Disk for the tree. Default in the console is 20 |
| `image` | no | Container image; default `ghcr.io/kloudlite/kloudlite-workspace` |
| `repo`, `branch` | no | Cloned into the tree on first start with your platform ssh key |
| `packages` | no | nixpkgs attributes, `attr` or `attr@version` — see [Packages](packages.md) |
| `team` | no | A team slug you belong to; the workspace then belongs to the team |

## Response

The workspace document. Poll `GET /v1/workspaces/{id}` until `state` is `ready`.

```json
{
  "id": "ws-7f3a…",
  "owner": "karthik",
  "team": "",
  "name": "api",
  "region": "centralindia-k3s",
  "state": "creating",
  "image": "ghcr.io/kloudlite/kloudlite-workspace",
  "placement": null,
  "quota_gb": 20,
  "packages": ["nodejs@22", "pnpm", "postgresql_16"],
  "base_packages": ["git", "openssh", "…"],
  "ssh": null
}
```

Once ready, `ssh` carries the gateway address and the host key to pin, and `placement` names the node.

## Errors

| Status | Why |
|---|---|
| `400` | Bad name, unknown region, a package entry that does not parse |
| `409` | Over quota — the body names the dimension: `workspaces: 5 of 5 in use; request more under Quota` |
| `422` | A pinned package no index knows, or one no binary cache holds; the body names the nearest versions |
| `503` | Every package index was unreachable and nothing was cached; nothing was written |

## Next steps

::: cards
- [ssh into it](ssh.md) — Terminal, editor, or agent.
- [Packages](packages.md) — What a pin means and how to update one.
- [Lifecycle](lifecycle.md) — Stop, start, delete.
:::
