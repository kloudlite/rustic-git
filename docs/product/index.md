# Introduction

Kloudlite runs development workspaces and application environments on Kubernetes regions you control, and connects the two over the network. You edit in a workspace. The application runs in an environment. An intercept routes one service's traffic to your workspace, so a change is observed without a build, a push, or a deploy.

Every object is reachable three ways with the same semantics: the web console, the `kl-connect` CLI on your machine, and the `/v1` HTTP API. Agents use the API and `ssh`; people use whichever is closest to hand.

## Get started

Create a workspace, connect to it, and run something in under five minutes.

::: cards
- [Quick start](quick-start.md) — Sign in, create a workspace, ssh in, run a service.
- [Authentication](authentication.md) — Tokens, ssh keys, and what each one may do.
:::

## What you get

| Object | What it is |
|---|---|
| [Workspace](concepts/workspaces.md) | A running sandbox with your source tree, a Nix toolchain, and ssh access. One per change you are making. |
| [Environment](concepts/environments.md) | Your application: a set of services with their data, owned by you or a team. |
| [Snapshot](concepts/snapshots.md) | A point-in-time copy of a workspace's tree or an environment's data. Restore, clone, or keep it. |
| [Connection](connections/attach.md) | A workspace attached to an environment resolves its services by name. An [intercept](connections/intercepts.md) delivers one service's traffic to the workspace instead. |
| Git and images | Repositories at `git.khost.dev` and an OCI registry at `cr.khost.dev`, under your handle or a team's. |

## Interfaces

| Surface | Where |
|---|---|
| Console | `https://dev.kloudlite.io` |
| API | `https://dev.kloudlite.io/v1`, bearer token — see [Authentication](authentication.md) |
| `kl-connect` | On your machine: sign in, list workspaces, ssh, write `~/.ssh/config` — [reference](reference/cli/kl-connect.md) |
| `kl` | Inside a workspace: `kl build`, `kl push` — [reference](reference/cli/kl.md) |
| Git | `git@git.khost.dev:{owner}/{repo}` with the ssh key on your account |

## Next steps

::: cards
- [Workspaces](concepts/workspaces.md) — How a workspace is built, stored, and placed.
- [Environments](concepts/environments.md) — Services, data, DNS, and what a stop keeps.
- [Agent tools](agent-tools/exec.md) — Run commands, move files, build images from an agent.
- [API reference](reference/api/index.md) — Every `/v1` route with request and response shapes.
:::
