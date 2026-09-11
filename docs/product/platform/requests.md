# Requests

Anything that must be granted is a request an administrator decides: a quota raise, access to a team, a region, or anything else. One pending request per owner per kind.

## Open one

```bash [API]
curl -sS https://dev.kloudlite.io/v1/requests -H "Authorization: Bearer $KL_TOKEN" \
  -H 'content-type: application/json' \
  -d '{
    "kind": "quota",
    "reason": "Two more environments for the staging split",
    "quota": { "environments": 4, "disk_gb": 200 }
  }'
```

| Kind | Payload | Effect on approval |
|---|---|---|
| `quota` | `quota: {workspaces?, environments?, snapshots?, disk_gb?, cpu?, memory_gb?}` | The owner's quota is raised |
| `access` | `access: {team, role}` | Membership is granted |
| `region` | `region: {region, title, body}` | Recorded; the region is enabled for the owner |
| `other` | `other: {title, body}` | Resolved with a written answer |

`owner` defaults to you; a team admin sets it to the team's slug.

## Follow it

```bash [API]
curl -sS https://dev.kloudlite.io/v1/requests -H "Authorization: Bearer $KL_TOKEN"
curl -sS https://dev.kloudlite.io/v1/requests/$ID -H "Authorization: Bearer $KL_TOKEN"
```

A request carries `state`, who opened it, and the resolution once decided. The effect is always written before the request is marked decided, so a request that reads approved has already taken effect.
