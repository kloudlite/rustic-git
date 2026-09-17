---
name: agents
description: Use when work can run in parallel, does not need this conversation's context, or is risky enough to want its own clone
---

# Agents

An agent is a fresh session with one task, working in a workspace, reporting back once. It has no
history of this conversation — whatever it needs to know goes in the brief — and it cannot start
agents of its own.

Start one with `ask {to: "agent", task: "…", name: "…"}`; close it with `kl_agent_close {name}`.
Several run at once, and you carry on meanwhile. Its answer arrives as a message:
`[from agent <name>] …`.

Use one for work that does not need your context: a test run, a survey of a codebase, a fix in
another workspace. Keep its conclusion, not its transcript.

`ask {to: "<workspace>"}` is the other shape: that workspace's OWN session, which remembers
everything it has done before. A teammate, not an agent.

## Where an agent works

By default it works in YOUR workspace — fine for reading, running tests, a small edit. Pass
`isolated: true` and it gets its own ephemeral clone (`<ws>-eph-<hex>`), a full copy with the same
packages. Use that when two or more agents change files at once, when the change might break the
working copy (a big refactor, a dependency upgrade, an experiment), or when you keep working
meanwhile. The agent says what it changed and where; take what you want from it — push from the
clone, or ask it to open a pull request — then `ask_close {name}`, which deletes the clone with it.
Never leave clones lying around.

    ask {to: "agent", name: "upgrade", isolated: true, task: "Upgrade to Svelte 5 and make the tests pass. Answer with what broke."}
    ask {to: "agent", name: "audit", task: "List every route with no auth check. Answer with the list."}
