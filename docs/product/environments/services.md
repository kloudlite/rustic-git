# Services

A service is one container with a stable name. The name is its DNS name inside the environment and the name every other object refers to it by.

## Fields

| Field | Rule |
|---|---|
| `name` | DNS label: `[a-z]([-a-z0-9]*[a-z0-9])?`, at most 63 characters |
| `image` | Any image the region can pull; `cr.khost.dev/{owner}/{name}:{tag}` for your own |
| `command` | Entrypoint override; `[]` keeps the image's |
| `env` | Map of environment variables; keys must be valid for a container |
| `ports` | TCP ports the service listens on; each becomes a port on its ClusterIP Service |
| `mounts` | `{folder, path}`: a folder on the environment's disk at an absolute path in the container |
| `resources` | `cpu_request`, `cpu_limit`, `memory_request`, `memory_limit`; absent means the platform default |

Rules are checked on the request and refused with `400`, because the same mistake in the cluster would fail on every reconcile forever.

## DNS

Services resolve each other by bare name. A workspace [attached](../connections/attach.md) to the environment resolves them the same way.

```
postgres://postgres:dev@db:5432/postgres
http://api:8080
```

## Data

Every mount is a folder on the one environment disk. A [push](../snapshots/push.md) snapshots all of them at once, so a restore brings every service's data back to the same instant.

## Status

`GET /v1/environments/{id}` reports each service:

| Field | Meaning |
|---|---|
| `ready` | The pod is running and ready |
| `intercepted_by` | The workspace currently receiving this service's traffic, if any |
| `unreachable_since` | When an intercepting workspace stopped answering |

## Resources

Requests and limits are per service. CPU and memory across all your running services count against your [quota](../platform/quota.md), and a namespace-level ResourceQuota is the hard stop.
