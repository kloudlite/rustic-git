# Teams and quota

Objects belong to an owner: your handle, or a team's slug. Membership decides what you may act on. Quota decides how much an owner may hold.

## Owners

| Owner | Who may act | Where it appears |
|---|---|---|
| You | You | `/{handle}/workspaces`, `git.khost.dev:{handle}/…`, `cr.khost.dev/{handle}/…` |
| A team | Every member | `/{team}/workspaces`, `git.khost.dev:{team}/…`, `cr.khost.dev/{team}/…` |

A workspace created with `team` set belongs to the team. Its `owner` field stays the person who made it; access follows the team.

## Quota

Every owner has six ceilings: workspaces, environments, snapshots, disk (GB), cpu, memory (GB). Usage is computed from what exists at the moment of every request, never from a counter, so it is always exact.

| Dimension | Person | Team |
|---|---|---|
| Workspaces | 5 | 20 |
| Environments | 2 | 8 |
| Snapshots | 20 | 80 |
| Disk | 100 GB | 400 GB |
| CPU | 40 | 148 |
| Memory | 80 GB | 296 GB |

A create, restore, clone, or push that would exceed a ceiling is refused with `409` and one sentence naming the dimension:

```
disk_gb: 96 of 100 in use; request more under Quota
```

## Requests

A quota raise, access to a team, or a new region is a request an administrator decides. One pending request per owner per kind. See [Requests](../platform/requests.md).
