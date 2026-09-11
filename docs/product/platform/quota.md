# Quota

Every owner has six ceilings. Usage is computed from what exists at the moment of every request, never from a counter, so it is always exact.

```bash [API]
curl -sS https://dev.kloudlite.io/v1/quota -H "Authorization: Bearer $KL_TOKEN"
```

```json
{
  "owner": "karthik",
  "limit": { "workspaces": 5, "environments": 2, "snapshots": 20, "disk_gb": 100, "cpu": 40, "memory_gb": 80 },
  "used":  { "workspaces": 2, "environments": 1, "snapshots": 6,  "disk_gb": 58,  "cpu": 6,  "memory_gb": 12 }
}
```

## Defaults

| Dimension | Person | Team |
|---|---|---|
| Workspaces | 5 | 20 |
| Environments | 2 | 8 |
| Snapshots | 20 | 80 |
| Disk | 100 GB | 400 GB |
| CPU | 40 | 148 |
| Memory | 80 GB | 296 GB |

## What counts

| Dimension | Counted |
|---|---|
| Workspaces, environments | Every one that exists, running or stopped |
| Snapshots | Every pushed snapshot, attached or detached |
| Disk | The sum of every working copy's `quota_gb` plus every snapshot's, plus the builder's cache |
| CPU, memory | Running workspaces and services, plus the builder while it runs |

## Refusals

A create, restore, clone, or push over a ceiling is `409` with one sentence:

```
snapshots: 20 of 20 in use; request more under Quota
```

CPU and memory have a second, hard stop in the cluster itself, so two requests racing past the check cannot both land.

## Raising it

A [request](requests.md) of kind `quota`.
