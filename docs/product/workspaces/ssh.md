# ssh

Every workspace runs an ssh server as user `kl`. Your account's keys open it. Connections go through the region's gateway with a short-lived ticket, never a public port on the workspace.

## From your machine

```bash [kl-connect]
kl-connect ws ssh api                 # by name or id
kl-connect ws ssh api -- -A -L 3000:localhost:3000
```

Write an ssh config once so `ssh api` and every editor work:

```bash [kl-connect]
kl-connect ws ssh-config
# Wrote ~/.ssh/kloudlite_config (3 workspaces).
ssh api
scp ./fixture.sql api:/home/kl/workspaces/api/
```

The file is included from `~/.ssh/config` and holds one `Host {name}` block per workspace with a `ProxyCommand kl-connect ws proxy {id}`. Re-run it after creating a workspace.

## From an agent

An agent runs commands over the same ssh, using a key on the account it acts for:

```bash
ssh -o BatchMode=yes api 'cd ~/workspaces/api && pnpm test'
rsync -az ./patch/ api:/home/kl/workspaces/api/
```

See [Agent tools](../agent-tools/exec.md).

## How a connection is made

1. `kl-connect` calls `POST /v1/workspaces/{id}/ssh-session` and gets a ticket: a signed token, the gateway address, and the host key to pin.
2. The ProxyCommand dials the gateway with the ticket; the gateway verifies it and splices the connection to the workspace.
3. The host key is checked against the ticket's, so a changed key is a refused connection, not a prompt.

Nothing is stored server-side per session.

## Keys

Keys belong to you and reach every workspace you may act on. Adding or revoking one takes effect on running workspaces without a restart. See [Authentication](../authentication.md).
