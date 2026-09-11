# Attach a workspace to an environment

An attached workspace resolves the environment's services by bare name, exactly as the services resolve each other. Attach and detach take effect on a running workspace without a restart.

::: tabs
```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/workspaces/$WS/attach \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{"environment": "env-2b8c…"}'
```
```text [Console]
Workspace → Connections → Attach → pick an environment
```
:::

Then, inside the workspace:

```bash
psql postgres://postgres:dev@db:5432/postgres
curl http://api:8080/health
```

## Rules

- One environment per workspace. Attaching to another replaces the first.
- The environment must be yours or a team's you belong to.
- Detach with `POST /v1/workspaces/{id}/detach`. Deleting the workspace detaches it.

## How it works

The platform renders a resolver configuration per workspace that points at the environment's namespace, and opens a network path between the workspace pod and the environment's services. Nothing runs inside your workspace for this, and no proxy sits in the path.

## Next step

Take over one of the environment's services so its traffic lands in the workspace: [Intercepts](intercepts.md).
