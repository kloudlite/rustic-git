# Environments

An environment is where your application runs.

It is a namespaced group of running services — normally every service of your
application, deployed and talking to each other. For the shop, an environment is `web`,
`api`, `payments`, `worker`, Postgres, and the queue, all up and wired together.
Code is not edited there. Editing happens in a [workspace](workspaces.md), which
[connects](connections.md) to the environment.

## The gap it closes

To see the refund fix work, you need somewhere realistic to run it: `api` calling
`payments`, Postgres holding an order big enough to refund, the queue delivering
the refund event to `worker`. Getting that has always meant one of two
compromises. A docker-compose stack on your laptop, which is never quite the real
shop and takes a day to set up. Or the shared staging environment, which is real
but belongs to everyone — so you wait your turn, avoid touching data, and find out
later that your test order broke someone else's run.

An environment is the real shop, and it is yours.

## One per developer

An environment is owned by one developer. It is your own copy of the whole
application, which you can break, intercept, reset, and take apart without affecting
anyone else.

Ownership is not a permission model; it is what makes the environment usable.
When you intercept `payments`, *all* `payments` traffic in that environment goes
to your workspace. That is only reasonable if the environment is yours. And an
application you can freely break is only useful if it is nobody else's.

Owned does not mean sealed. Other developers can connect their workspaces to your
environment — see [below](#one-workspace-different-environments).

## Cloning

Nobody assembles an environment by hand. A developer who needs one clones one.
The clone is a full copy of the service group, owned by whoever cloned it.

When a new developer joins the team and needs a shop of their own, they clone
yours. When you want a second one — kept stable while you take the first apart —
you clone your own. When a test run is going to write to Postgres, it gets a clone
too, and discards it afterwards.

Cloning is cheap for the same reason [snapshots](snapshots.md) are: the state is
copied, not prepared.

## One workspace, different environments

A workspace is not tied to one environment. You can disconnect your `payments`
workspace from your environment and connect it to another. Your code stays put;
the shop around it changes.

- **Testing.** A teammate's environment has different data — a customer with a
  partial refund already in progress. Connect to it to see whether your fix holds
  there too.
- **Collaboration.** A teammate wants to see the fix before it merges. Connect to
  their environment and intercept `payments` there. Their shop, your code,
  immediately — no branch for them to check out and nothing to deploy. When you
  disconnect, their environment is exactly as it was.

## Lifecycle

- **Create** — a new environment with its services.
- **Update** — change what runs in the environment and how it is configured.
- **Clone** — a copy of an existing environment, owned by the caller; from the
  environment as it stands, or from one of its [snapshots](snapshots.md).
- **Snapshot** — capture all persisted data in the environment as a single object.

## Where next

- [Snapshots](snapshots.md) — why state stops being scarce.
- [Best practices: environments](../best-practices.md#environments)
- [Environment API](../reference/api/environments.md)
