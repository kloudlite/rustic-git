---
name: workspaces
description: Use when the person wants a new machine, another project or language, or asks what workspaces exist or to start/stop/clone/delete one
---

# Workspaces

A workspace is a machine: a disk, packages, a shell, and a place your files stay between sessions.
You are one. Another person's work, another project, another language — each gets its own.

Verbs: `kl_workspaces` (what exists), `kl_workspace` (one in full), `kl_workspace_create`
(empty, from a repo and branch, or `from_snapshot`), `kl_workspace_start`, `kl_workspace_stop`,
`kl_workspace_clone` (a copy to try something in), `kl_workspace_delete`.
A workspace's own packages: `kl_pkg_list`, `kl_pkg_add`, `kl_pkg_rm` — `attr` or `attr@version`.
Packages are installed in a WORKSPACE. There is no "on the bench" to install into: a bench session
has no machine of its own, so a package request names the workspace it is for.
What another one is doing: `kl_workspace_progress`.

## Packages

Packages are **nixpkgs attribute names**, not language names. `rust` is not a package; `rustc` and
`cargo` are. `attr@version` pins one (`nodejs_22@22.11.0`), and an unknown attribute is refused by
the api with the nearest ones named — read that sentence back to the person rather than guessing
again.

| toolchain | attributes |
| --- | --- |
| Rust | `rustc`, `cargo` (and `rust-analyzer` for an editor) |
| Node | `nodejs_22`, plus `pnpm` or `bun` |
| Go | `go` |
| Python | `python3`, `uv` |
| Java | `jdk21` |
| C / C++ | `gcc`, `gnumake` |

A workspace is named. Ids work too, but a name is what a person says, and every tool takes both.
Starting and stopping take a while; the tool waits and answers with the final state.

Example — a Go backend of its own, then the work sent there:

    kl_workspace_create {name: "svelte-backend", packages: ["go"]}
    ask {to: "svelte-backend", task: "add a /healthz endpoint and run the tests"}
