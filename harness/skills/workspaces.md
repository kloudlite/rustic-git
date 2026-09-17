# Workspaces

A workspace is a machine: a disk, packages, a shell, and a place your files stay between sessions.
You are one. Another person's work, another project, another language — each gets its own.

Verbs: `kl_workspaces` (what exists), `kl_workspace` (one in full), `kl_workspace_create`
(empty, from a repo and branch, or `from_snapshot`), `kl_workspace_start`, `kl_workspace_stop`,
`kl_workspace_clone` (a copy to try something in), `kl_workspace_delete`.
Your own packages: `kl_pkg_list`, `kl_pkg_add`, `kl_pkg_rm` — `attr` or `attr@version`.
What another one is doing: `kl_workspace_progress`.

A workspace is named. Ids work too, but a name is what a person says, and every tool takes both.
Starting and stopping take a while; the tool waits and answers with the final state.

Example — a Go backend of its own, then the work sent there:

    kl_workspace_create {name: "svelte-backend", packages: ["go"]}
    ask {to: "svelte-backend", task: "add a /healthz endpoint and run the tests"}
