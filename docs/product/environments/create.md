# Create an environment

`POST /v1/environments` with a name, a region, a list of services, and a disk quota. The response is the environment in `creating`; it is `running` once every service is up.

## Request

::: tabs
```bash [API]
curl -sS https://dev.kloudlite.io/v1/environments \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{
    "name": "acme-dev",
    "region": "centralindia-k3s",
    "quota_gb": 20,
    "owner": "acme",
    "services": [
      {
        "name": "db",
        "image": "postgres:16",
        "command": [],
        "env": { "POSTGRES_PASSWORD": "dev" },
        "ports": [5432],
        "mounts": [{ "folder": "pgdata", "path": "/var/lib/postgresql/data" }]
      },
      {
        "name": "api",
        "image": "cr.khost.dev/acme/api:1",
        "command": ["node", "server.js"],
        "env": { "DATABASE_URL": "postgres://postgres:dev@db:5432/postgres" },
        "ports": [8080],
        "mounts": []
      }
    ]
  }'
```
```text [Console]
Environments → New environment
Add services one at a time; the same fields as the API.
```
:::

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Environment name, unique per owner |
| `region` | yes | A region id from `GET /v1/regions` |
| `services` | yes | At least one — see [Services](services.md) |
| `quota_gb` | yes | Disk for every mount together; default 20 |
| `owner` | no | A team slug you belong to; default is you |

## Response

```json
{
  "id": "env-2b8c…",
  "owner": "acme",
  "name": "acme-dev",
  "region": "centralindia-k3s",
  "state": "creating",
  "placement": null,
  "services": [ … ],
  "restored_to": null
}
```

Poll `GET /v1/environments/{id}` until `state` is `running`. Each service's status carries its readiness and, when intercepted, which workspace holds it.

## Errors

| Status | Why |
|---|---|
| `400` | A service name that is not a DNS label, a mount path that is not absolute, a port of 0, an env key the API server would refuse |
| `409` | Over quota; the body names the dimension |
