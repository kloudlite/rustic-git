---
name: agents
description: Use when work can run in parallel, does not need this conversation's context, or is risky enough to want its own working directory
---

# Agents

An agent is a fresh session with one task, reporting back once. It has no history of this
conversation — whatever it needs goes in the brief — and it cannot start agents of its own.

Start one with `ask {to: "agent", task: "…", name: "…"}`; close it with `ask_close {name}`.
Several run at once, and you carry on meanwhile. Its answer arrives as a message:
`[from agent <name>] …`.

Use one for work that does not need your context: a test run, a survey of a codebase, a fix in
another workspace. When to reach for one, and what comes back, is in your identity already; this
skill is only the shape of one.

`ask {to: "<workspace>"}` is the other shape: that workspace's OWN session, which remembers
everything it has done before. A teammate, not an agent.

## Where an agent works

An agent always works in a workspace. From the bench, name it — `workspace:` — because the bench has
no machine of its own; from a workspace session you may leave it out and it works in that one.

Every agent gets its own **tree**: a writable copy of that workspace's working directory, inside the
same machine, cut the moment it is dispatched. Two agents changing files at once cannot trip over
each other, a refactor that goes wrong is thrown away with the tree, and the caches are already
warm, so a build there is as fast as one in the workspace itself. There is no second machine and
nothing to wait for beyond the cut.

Its ports are a block of its own; a command it runs is given `PORT` and `KL_PORT_RANGE`, and the
rest belong to the workspace's own working directory.

It commits on a branch named after itself and pushes (or opens a pull request), then reports with
the branch or the pull. Its tree and its transcript STAY whatever the outcome: you read the report,
open the diff, and close it with `ask_close {name}` once the work is merged and clear — which is
the one thing that deletes them.

    ask {to: "agent", name: "upgrade", workspace: "svelte-frontend", task: "Upgrade to Svelte 5 and make the tests pass. Answer with what broke and the branch."}
    ask {to: "agent", name: "audit", workspace: "api", task: "List every route with no auth check."}

- Reporting back: the outcome, what changed for the person in capability terms, the `contracts:` line, and what is needed next — never a file, a path, a command or a digest, which stay in the tree you worked in.
