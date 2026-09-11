# Teams

A team is an owner with members. Its workspaces, environments, repositories, and images sit under the team's slug, and every member may act on them.

## Create

```bash [API]
curl -sS https://dev.kloudlite.io/v1/teams -H "Authorization: Bearer $KL_TOKEN" \
  -H 'content-type: application/json' -d '{"slug": "acme", "name": "Acme"}'
```

## Invite

```bash [API]
curl -sS https://dev.kloudlite.io/v1/teams/acme/invites -H "Authorization: Bearer $KL_TOKEN" \
  -H 'content-type: application/json' -d '{"email": "dev@acme.example", "role": "member"}'
curl -sS -X DELETE https://dev.kloudlite.io/v1/teams/acme/invites/$INVITE -H "Authorization: Bearer $KL_TOKEN"
```

Roles are `member` and `admin`. An admin may open [requests](requests.md) for the team.

## Owning objects as a team

| Object | Field |
|---|---|
| Workspace | `team` on create |
| Environment | `owner` on create |
| Image | `kl build -t acme/api:1 .` |
| Repository | `git@git.khost.dev:acme/{repo}` |

Access follows membership at request time. Removing a member removes their access to everything the team owns, including running workspaces' ssh, without a restart.

## Profile

`GET /v1/teams/{slug}/profile` is the public profile; `GET /v1/teams` lists yours.
