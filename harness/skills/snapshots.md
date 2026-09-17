# Snapshots

A snapshot is a workspace or an environment as it was at one moment — the whole disk, kept until
somebody deletes it. It is how you go back, and how you hand a starting point to someone else.

Verbs: `kl_workspace_snapshot` / `kl_environment_snapshot` (take one, with a message saying what it
is), `kl_workspace_snapshots` / `kl_environment_snapshots` (what has been taken, newest first),
`kl_environment_restore` (put an environment back to one), and `from_snapshot` on either create
(a NEW workspace or environment from that moment).

Take one before anything you might want to undo. The message is what a person reads later, so write
it for them: "before the auth refactor", not "snapshot 3".

Example:

    kl_workspace_snapshot {id: "svelte-backend", message: "before the auth refactor"}
    kl_workspace_create {name: "auth-try", from_snapshot: "snap-2f9c"}
