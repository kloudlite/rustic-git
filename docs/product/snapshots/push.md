# Push

`push` snapshots a workspace's tree or an environment's disk and keeps it until you delete it. It is the only verb that adds to [history](history.md).

::: tabs
```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/workspaces/$WS/push \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{"message": "before the schema migration"}'
```
```bash [API · environment]
curl -sS -X POST https://dev.kloudlite.io/v1/environments/$ENV/push \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{"message": "seeded with prod-like data"}'
```
:::

The body is optional. The response is the snapshot record; `phase` is `Pending` until the node has cut and replicated it, then `Ready`.

## What a push records

- The bytes, as a read-only btrfs snapshot on the node, replicated to the volume's other holders.
- The parent: the previous head, so history is a chain.
- The definition at that instant: image, packages and their locks, quota, attached environment for a workspace; services and quota for an environment. A restore defaults to it.
- Your message.

## What it does not do

- It does not stop or pause anything. A push of a running workspace is a consistent point-in-time cut of the filesystem.
- It does not upload to an object store. Durability is replica count on the region's nodes.
- It is not git. Commit and push your code with git as usual; a snapshot is the whole tree, including what git ignores.

## Limits

Snapshots count against your quota (20 for a person, 80 for a team by default). A push over the ceiling is `409`.
