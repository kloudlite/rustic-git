# Environments

An environment is your application: a named set of services, each an image with a command, ports, environment variables, and mounts, plus one disk that holds every service's data. It runs in its own namespace, so services reach each other by name.

## Services

A service is one StatefulSet with one pod. Its name is its DNS name inside the environment, so `mongodb://db:27017` resolves when a service is called `db`. A name is a DNS label: lowercase letters, digits, hyphens, at most 63 characters.

```json
{
  "name": "db",
  "image": "postgres:16",
  "env": { "POSTGRES_PASSWORD": "dev" },
  "ports": [5432],
  "mounts": [{ "folder": "pgdata", "path": "/var/lib/postgresql/data" }],
  "resources": { "cpu_request": "500m", "cpu_limit": "1", "memory_request": "1Gi", "memory_limit": "2Gi" }
}
```

A mount names a folder on the environment's disk and the path it appears at in the container. Every service's folders live on the one disk, which is why a snapshot of an environment is one consistent cut across all its services.

## State

| State | Meaning |
|---|---|
| `creating` | Placed; StatefulSets being applied |
| `running` | Every service is up |
| `stopped` | Services torn down after a final sync point; the disk is kept |
| `error` | A service will not converge; the service's own status says why |

## Ownership

An environment belongs to you or to a team. A team's environment is visible to every member, and a member's workspace may attach to it.

## Data

The disk is a btrfs subvolume, quota-bounded by `quota_gb`, replicated like a workspace's tree. A push takes a snapshot of the whole disk. A restore puts a snapshot's bytes under a new environment, or in place under the same one.

## Next steps

::: cards
- [Create an environment](../environments/create.md) — Define services and their mounts.
- [Services](../environments/services.md) — Validation rules, DNS, resources, and status.
- [Clone and restore](../environments/clone-and-restore.md) — Copy an environment, or bring back a snapshot.
:::
