# kl

The CLI inside a workspace. Builds run on your builder and push to your registry.

```
kl build -t NAME[:TAG]... [-f FILE] [--build-arg K=V]... [--platform P] [--no-cache] [CONTEXT]
kl push SRC DST...
kl ide serve [--bind 127.0.0.1:7788] [--graft-dir DIR]
```

## `kl ide serve`

The workspace tool server, started by the pod before sshd. Loopback only; reach it with `kl-connect ws ide`. See [Tool server](../../agent-tools/ide-server.md). It refuses to start unless it runs as `kl`, `KL_WORKSPACE` names an existing directory, and `~/.config/git/ignore` carries the platform block.

## `kl build`

| Flag | Meaning |
|---|---|
| `-t, --tag` | Required, repeatable. `hello:1` is `cr.khost.dev/{you}/hello:1`; `team/hello:1` pushes under the team |
| `-f, --file` | Dockerfile path |
| `--build-arg` | Repeatable |
| `--platform` | Target platform |
| `--no-cache` | Skip the builder's cache |
| `CONTEXT` | Default `.` |

The image is pushed as the build finishes. There is no local image and no separate push step.

## `kl push`

Copies an image the registry already holds to one or more new names. Registry-side; nothing is downloaded.

```bash
kl push hello:1 hello:latest team/hello:1
```

## Environment

`kl` reads the workspace's projected registry credential and the builder address from the environment the platform sets. Run outside a workspace it exits with `… is not set — kl runs inside a kloudlite workspace`.
