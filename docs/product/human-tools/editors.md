# Editors

Any editor with an ssh remote mode works with a workspace once `kl-connect ws ssh-config` has been run. The host is the workspace's name.

## VS Code

```bash
code --remote ssh-remote+api /home/kl/workspaces/api
```

Or Remote-SSH → Connect to Host → `api`. The remote server installs into the workspace's local cache, not the shared home, so two workspaces on different nodes never race it.

## Cursor, Windsurf, and forks

Same as VS Code: Remote-SSH with host `api`.

## JetBrains

Gateway → SSH → host `api`, user `kl`, project `/home/kl/workspaces/api`. The IDE backend goes to the workspace's local cache.

## Terminal editors

```bash
ssh api
```

Your dotfiles are in `/home/kl` and persist across every workspace in the region, so a configured shell, `vim`, or `nvim` follows you.

## Port forwarding

```bash
ssh -L 3000:localhost:3000 api
```

Or let the editor forward ports; both go over the same tunnel.
