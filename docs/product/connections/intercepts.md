# Intercepts

An intercept makes one service's traffic go to your workspace. Everything in the environment keeps dialling `api:8080`; the bytes arrive at the process you are running, on the port you choose. No rebuild, no redeploy, no proxy.

## Set an intercept

The workspace must be [attached](attach.md) to the environment.

::: tabs
```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/environments/$ENV/intercepts \
  -H "Authorization: Bearer $KL_TOKEN" -H 'content-type: application/json' \
  -d '{
    "service": "api",
    "workspace": "ws-7f3a…",
    "ports": [{ "service": 8080, "workspace": 3000 }]
  }'
```
```text [Console]
Environment → Services → api → Intercept → pick the workspace and the port map
```
:::

`ports` maps a port callers dial on the service to the port your process listens on in the workspace. Then run your dev server:

```bash
pnpm dev --port 3000
```

Every request another service sends to `api:8080` now reaches it.

## What happens in the environment

- The real service is scaled to zero, so a queue consumer in the old pod cannot eat messages your workspace never sees.
- The service's address and DNS name are unchanged; callers change nothing.
- Its endpoints point at your workspace. Traffic is routed by endpoints, never by a proxy in the path.

## Release

```bash [API]
curl -sS -X DELETE https://dev.kloudlite.io/v1/environments/$ENV/intercepts/api \
  -H "Authorization: Bearer $KL_TOKEN"
```

The real service scales back up and its endpoints are restored.

## Wish and effect

The intercept you set is a wish on the environment. What is in force is each service's `intercepted_by`, and the console shows only that. The two differ in one case: your workspace stops answering (stopped, deleted, node down, or its pod restarts and stays down past a grace period). Then the real service comes back on its own, the wish stays, and it takes hold again when the workspace returns. Only the `DELETE` above removes a wish.

## Rules

- One workspace per service. A second request is refused naming the holder.
- The workspace and the environment must be in the same region.
- An ordinary pod restart in the workspace does not bounce the service; only an outage past the grace period does.
