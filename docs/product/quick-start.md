# Quick start

Create a workspace, connect to it, and run a service. Everything here is the same object seen from the console, the CLI, and the API; pick the tab you will use.

## 1. Sign in

Sign in at `https://dev.kloudlite.io` with GitHub or Google. Then install `kl-connect` and log in. It opens the browser once, stores a CLI token under `~/.config/kl-connect`, and prints where the token went.

```bash [kl-connect]
kl-connect login
```

An API call needs a bearer token. `kl-connect login` stores one; `kl-connect ws list` shows it is working. See [Authentication](authentication.md) for minting a token by hand.

## 2. Create a workspace

A workspace needs a name, a region, and a disk quota. Packages are Nix attribute names, optionally pinned as `attr@version`.

::: tabs
```bash [kl-connect]
# The CLI creates through the console today; open it:
open https://dev.kloudlite.io/me/workspaces
```
```bash [API]
curl -sS https://dev.kloudlite.io/v1/workspaces \
  -H "Authorization: Bearer $KL_TOKEN" \
  -H 'content-type: application/json' \
  -d '{
    "name": "api",
    "region": "centralindia-k3s",
    "quota_gb": 20,
    "repo": "git@github.com:acme/api.git",
    "branch": "main",
    "packages": ["nodejs@22", "pnpm"]
  }'
```
:::

The response is the workspace document with `state: "creating"`. It becomes `ready` once the pod is running and the packages are published; `GET /v1/workspaces/{id}` reports progress under `packages_status` and `ssh`.

## 3. Connect

`kl-connect ws ssh` resolves a name or id, fetches a short-lived connect ticket, and opens ssh through the region's gateway. Anything after `--` goes to ssh.

```bash [kl-connect]
kl-connect ws list
kl-connect ws ssh api
kl-connect ws ssh api -- -A          # forward your agent
```

Write an ssh config once and every editor's remote mode works with the workspace's name as the host:

```bash [kl-connect]
kl-connect ws ssh-config
ssh api
code --remote ssh-remote+api /home/kl/workspaces/api
```

## 4. Run something

Inside the workspace your tree is at `/home/kl/workspaces/api`, your home persists across restarts and across workspaces in the region, and the packages you asked for are on `PATH`.

```bash
cd ~/src
pnpm install
pnpm dev --port 3000
```

## 5. Stop, and keep the tree

A stop cuts a sync point, tears the pod down, and keeps the tree. A start brings it back on the same bytes.

::: tabs
```bash [API]
curl -sS -X POST https://dev.kloudlite.io/v1/workspaces/$WS/stop -H "Authorization: Bearer $KL_TOKEN"
curl -sS -X POST https://dev.kloudlite.io/v1/workspaces/$WS/start -H "Authorization: Bearer $KL_TOKEN"
```
:::

## Next steps

::: cards
- [Create an environment](environments/create.md) — Run your application's services with their data.
- [Attach and intercept](connections/attach.md) — Reach an environment's services from the workspace by name, then take one over.
- [Packages](workspaces/packages.md) — Pin tool versions, update them, and what a pin guarantees.
- [Editors](human-tools/editors.md) — VS Code, JetBrains, and any ssh-capable editor.
:::
