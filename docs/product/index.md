# Kloudlite

Kloudlite runs your development workspaces and your application environments on a Kubernetes
region you control, and connects the two over the network. You edit in a workspace, the
application runs in an environment, and an intercept routes a service's traffic to your workspace
so a change is observed without a build, a push, or a deploy.

Everything is an HTTP API (`/v1`), the same one the web console and the CLI use.

## The objects

| Object | What it is | Backed by |
|---|---|---|
| **Workspace** | A running sandbox with your source, a Nix toolchain, and ssh access. One per thing you are changing. | one pod per workspace, a btrfs subvolume for the tree, a shared NFS home per person and region |
| **Environment** | Your application: a set of services with their data, owned by you or your team. | one namespace per environment, a StatefulSet per service, one btrfs subvolume for all data |
| **Snapshot** | A point-in-time copy of a workspace's tree or an environment's data. Restore into a new copy, or clone to work from it. | read-only btrfs snapshots, replicated across nodes |
| **Connection** | A workspace attached to an environment resolves its services by name. An **intercept** delivers one service's traffic to the workspace instead. | a per-workspace `resolv.conf` and NetworkPolicies; an EndpointSlice rewrite for intercepts |
| **Git repositories** and **container images** | Hosted on the same platform, under your handle or a team's. | the `kloudlite` server tier and the OCI registry at `cr.khost.dev` |

## Where to start

- [Your first workspace](tutorials/01-first-workspace.md): create one, ssh in, run a service.
- [Workspaces](concepts/workspaces.md), [Environments](concepts/environments.md),
  [Snapshots](concepts/snapshots.md), [Connections and intercepts](concepts/connections.md).
- [How-to guides](how-to/): one task per page, web console and API.
- [Reference](reference/): the `/v1` API, the CLI, limits and defaults, glossary.

## Interfaces

| | |
|---|---|
| Web console | `https://dev.kloudlite.io` |
| API | `https://dev.kloudlite.io/v1`, bearer token from `kl-connect login` |
| Laptop CLI | `kl-connect`: sign in, list workspaces, ssh into one, wire `~/.ssh/config` |
| In the workspace | `kl build` / `kl push`: build an image on your builder and push it to the registry |
| Git | `git@git.khost.dev:{owner}/{repo}` with the ssh key on your account |
