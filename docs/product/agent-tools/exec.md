# Run commands

An agent drives a workspace two ways. The [tool server](ide-server.md) inside every workspace exposes exec, files, watch and the code graph as a plain HTTP API through the ssh tunnel, typed and bounded. Plain ssh is the other: the same keys, the same audit path, and no second permission model. This page is the ssh way.

## One-off commands

```bash
ssh -o BatchMode=yes api 'cd ~/workspaces/api && pnpm test -- --reporter=json'
```

`BatchMode=yes` fails instead of prompting. Exit status and output are the command's own.

## Setting up ssh for an agent

1. Add the agent's public key to the account it acts for: [Authentication](../authentication.md#ssh-keys).
2. On the machine the agent runs on, install `kl-connect`, log in as that account, and write the config:

```bash [kl-connect]
kl-connect login
kl-connect ws ssh-config
```

3. Use the workspace's name as the ssh host.

The ProxyCommand fetches a fresh connect ticket per connection, so a long-running agent needs no token refresh logic of its own.

## Long-running processes

Start a process that outlives the session with a multiplexer or `nohup`; a workspace keeps running until it is stopped.

```bash
ssh api 'cd ~/workspaces/api && nohup pnpm dev --port 3000 > ~/dev.log 2>&1 &'
ssh api 'tail -n 50 ~/dev.log'
```

## Working directory

The tree is at `/home/kl/workspaces/{name}`; the home at `/home/kl` is shared across the account's workspaces in the region. Write scratch to the tree, not the home, unless you want every workspace to see it.

## Creating workspaces from an agent

Everything on `/v1` is a bearer call. A typical loop: create → poll `state` → `ssh-config` → work → `push` with a message → `stop` or `delete`.

```bash [API]
WS=$(curl -sS https://dev.kloudlite.io/v1/workspaces -H "Authorization: Bearer $KL_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name":"task-42","region":"centralindia-k3s","quota_gb":20,"repo":"git@github.com:acme/api.git","branch":"main","packages":["nodejs@22","pnpm"]}' | jq -r .id)
until [ "$(curl -sS https://dev.kloudlite.io/v1/workspaces/$WS -H "Authorization: Bearer $KL_TOKEN" | jq -r .state)" = ready ]; do sleep 3; done
kl-connect ws ssh-config
ssh -o BatchMode=yes task-42 'cd ~/workspaces/task-42 && pnpm install && pnpm test'
```
