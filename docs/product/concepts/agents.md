# Agents

Most operations in Kloudlite are not issued by a person. They are issued by AI
agents working on a developer's behalf: cloning workspaces, working inside them,
verifying against an environment, pushing the result, discarding. The developer
supervises a fleet.

Agents and developers work on the same objects. A developer opens a shell in a
workspace; an agent reaches the same workspace through
[read, write, exec, and background processes](workspaces.md#the-surface) — the
shell's capabilities, exposed as operations. Everything above the workspace —
create, clone, discard, connect, intercept, snapshot — is identical for both.
Agents just issue far more of it, in parallel, unattended.

That changes what the platform has to guarantee.

## An agent's afternoon

You hand an agent five tickets against `payments`, including the refund bug. Here
is the shape of what happens.

1. The agent clones five **ephemeral workspaces** from your `payments` workspace,
   one per ticket. Each is warm — dependencies installed, caches hot — and each is
   its own tree.
2. In each, it starts `payments` in the background under a reloader and a test
   watcher next to it.
3. For the refund ticket, it clones an environment from the
   `large-order-pending` snapshot, connects the workspace to it, and intercepts
   `payments` there. It now has a real shop, with a real order to refund, that
   nobody else is using.
4. It reads the code, writes the fix, and sees the test watcher go green and the
   refund request from `api` land in its code.
5. It commits, pushes to the working branch, discards the ephemeral workspace, and
   discards the environment clone.

Five of those run at once. None share a tree, none share data, and when they are
done, nothing is left behind but five branches.

## What agents need that people tolerate without

**Isolation by default.** You work on one thing at a time and can live with one
working tree and one environment. Five agents cannot. Every task gets its own
[ephemeral workspace](workspaces.md#ephemeral-workspaces); any task that writes
data gets its own environment [cloned from a snapshot](snapshots.md). Two agents
never share a tree, and never share state they can corrupt.

**Nothing to hold in context.** A build-and-deploy step is not only slow for an
agent; it is state to remember, poll, and re-establish, paid in tokens on every
iteration. [Intercept](connections.md) collapses that into "the process is running;
the next request hits it".

**One place to ask.** An agent cannot reconcile a git host, a registry, a cluster,
and an issue tracker by reading four dashboards. It needs one platform that already
knows how the ticket, the branch, the image, the workspace, and the environment
relate. That is why [the whole loop lives in Kloudlite](overview.md#the-whole-loop-in-one-place).

**Safe failure with nobody watching.** If the agent's workspace dies in step 4, the
environment [recovers on its own](connections.md#when-the-connection-ends) and
marks the dead intercept. If the agent is killed before step 5, the only thing
lost is what was not pushed — and the cycle pushes before it discards.

## The division of labour

Agents do the repeatable part: set up, branch per task, do the task, verify against
a real environment, push, clean up. The developer keeps what is not repeatable:
deciding what to build, reviewing what came back, owning the long-lived
workspaces and the environment the agents work against.

The developer's view is therefore not one workspace but many — a few long-lived
component workspaces they own, and a churn of ephemeral ones that agents create
and destroy beneath them.

## Where next

- [Best practices: agents](../best-practices.md#agents)
- [Tutorial: an agent-driven flow](../tutorials/02-agent-driven-flow.md)

<!-- Open: the agent-facing interface (same API/CLI as humans, MCP, SDK). The
     page holds regardless; add a "How agents call Kloudlite" section once
     decided. -->
