# kl-connect

The CLI on your machine. `kl-connect --help` prints the same.

```
kl-connect login [--api URL]        Log in through the browser and store the CLI token
kl-connect logout                   Revoke this machine's CLI token and forget it
kl-connect ws list [--team SLUG]    List your workspaces
kl-connect ws ssh TARGET [-- ARGS]  ssh into a workspace by name or id; ARGS go to ssh
kl-connect ws ssh-config            Write ~/.ssh/kloudlite_config and Include it from ~/.ssh/config
kl-connect ws proxy ID              ssh's ProxyCommand (used by the config; not for hand use)
kl-connect builder status [--team SLUG]
                                    The builder's state, readiness, and why it is not ready
```

## `login`

`--api` defaults to `https://dev.kloudlite.io`. Prints a code, opens the browser, waits for confirmation, stores the token.

## `ws list`

Columns `NAME`, `ID`, `STATE`, `PACKAGES`.

## `ws ssh`

`TARGET` is a name or an id; an exact id wins. Everything after `--` is passed to ssh unchanged.

```bash
kl-connect ws ssh api -- -A -L 3000:localhost:3000
```

## `ws ssh-config`

Writes one block per workspace:

```
Host api
  HostName ws-7f3a…
  User kl
  ProxyCommand kl-connect ws proxy ws-7f3a…
```

and adds `Include ~/.ssh/kloudlite_config` to `~/.ssh/config` once. Run it again after creating or renaming a workspace. `kl-connect` must be on `PATH` for ssh to find the ProxyCommand.

## Files

| Path | What |
|---|---|
| `~/.config/kl-connect/config.json` | API URL and token |
| `~/.config/kl-connect/known_hosts` | Pinned gateway host keys |
| `~/.ssh/kloudlite_config` | Generated host blocks |

`KL_CONFIG_DIR` moves the config directory.
