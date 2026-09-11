# kl-connect

`kl-connect` runs on your machine. It signs in, lists workspaces, opens ssh through the gateway, and writes an ssh config so every other tool can reach a workspace by name.

## Install

Download the binary for your platform from the releases page and put it on `PATH`. It must be on `PATH` for the ssh config to work, because ssh runs it as a ProxyCommand.

## Log in

```bash [kl-connect]
kl-connect login
# Confirm this code in your browser: XKQ4-7P
```

The browser confirms the code; the token lands under `~/.config/kl-connect/`. `kl-connect logout` revokes it and forgets it.

## Workspaces

```bash [kl-connect]
kl-connect ws list
kl-connect ws list --team acme
```

```
NAME                 ID                       STATE      PACKAGES
api                  ws-7f3a…                 ready      nodejs@22,pnpm,postgresql_16
```

```bash [kl-connect]
kl-connect ws ssh api
kl-connect ws ssh api -- -A -L 5432:db:5432
kl-connect ws ssh-config
```

## Tool server

```bash [kl-connect]
kl-connect ws ide api            # tunnels the workspace's tool server to localhost:7788
kl-connect ws ide api --port 7790
```

Then `claude mcp add --transport http workspace http://localhost:7788/mcp`. See [Tool server](../agent-tools/ide-server.md).

## Builder

```bash [kl-connect]
kl-connect builder status [--team acme]
```

Full flag reference: [CLI reference](../reference/cli/kl-connect.md).
