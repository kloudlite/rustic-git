# Editors

Any editor with an ssh remote mode works with a workspace once `kl-connect ws ssh-config` has been run. The host is the workspace's name.

## VS Code

```bash
code --remote ssh-remote+api /home/kl/workspace
```

Or Remote-SSH → Connect to Host → `api`. The remote server installs into the workspace's own cache under its home volume.

## Cursor, Windsurf, and forks

Same as VS Code: Remote-SSH with host `api`.

## JetBrains

Gateway → SSH → host `api`, user `kl`, project `/home/kl/workspace`. The IDE backend goes to the workspace's local cache.

## Terminal editors

```bash
ssh api
```

Your dotfiles are in `/home/kl`, this workspace's own volume, so a configured shell, `vim`, or `nvim` persists across stop and start of this workspace.

## Port forwarding

```bash
ssh -L 3000:localhost:3000 api
```

Or let the editor forward ports; both go over the same tunnel.
