# Create a Workspace

A workspace needs a name, a region, a disk quota, and optionally a repository to seed it from and
a package list. It is ready in a few seconds; the first start on a node that has never built the
same package set takes about 30 s longer.

## Web console

1. Open `/{owner}/workspaces` and choose **New workspace**.
2. Name it, pick the region, and optionally a repository and branch to clone into `/workspace`.
3. Add packages as nixpkgs attribute names, pinned or not (`nodejs@20`, `jq`).
4. Create. The list shows `creating` until the pod is up, then `ready`.

## API

```bash
curl -sS https://dev.kloudlite.io/v1/workspaces \
  -H "Authorization: Bearer $KL_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "payments",
    "region": "centralindia-k3s",
    "quota_gb": 20,
    "repo": "acme/payments",
    "branch": "main",
    "packages": ["nodejs@20", "postgresql@16", "jq"]
  }'
```

| Field | Required | Notes |
|---|---|---|
| `name` | yes | Shown in lists and used as the ssh host alias. |
| `region` | yes | One of `GET /v1/regions`. |
| `quota_gb` | yes | Size of `/workspace`. Counts against your `diskGb` quota. |
| `repo`, `branch` | no | A platform repository as `owner/name`, cloned over ssh with your platform key by an init container. `branch` is required with `repo`. Not a URL. |
| `packages` | no | nixpkgs attribute names, `attr` or `attr@version`. Unknown or uncached pins are refused with `422` and the nearest versions. |
| `team` | no | A team slug; the workspace belongs to the team and its quota. |
| `image` | no | Your own image instead of the default. No sshd is added; access is `exec` only. |

The response is the workspace document:

```json
{
  "id": "ws-4f1c9a2e8d3b7a10",
  "owner": "you",
  "team": "",
  "name": "payments",
  "region": "centralindia-k3s",
  "state": "creating",
  "image": "ghcr.io/kloudlite/kloudlite-workspace",
  "quota_gb": 20,
  "packages": ["nodejs@20", "postgresql@16", "jq"]
}
```

Poll `GET /v1/workspaces/{id}` until `state` is `ready`; `ssh` then carries the gateway address and
host key.

## Then

```bash
kl-connect ws ssh payments
```

Refusals: `409 workspaces: 5 of 5 in use; request more under Quota` when the owner's quota is
full; `422` naming the package entry that could not be resolved; `400 branch is required with
repo`.
