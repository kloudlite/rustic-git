---
name: agents
description: Use when work can run in parallel, does not need this conversation's context, or is risky enough to want its own clone
---

# Agents

An agent is a fresh session with one task, reporting back once. It has no history of this
conversation — whatever it needs goes in the brief — and it cannot start agents of its own.

Start one with `ask {to: "agent", task: "…", name: "…"}`; close it with `kl_agent_close {name}`.
Several run at once, and you carry on meanwhile. Its answer arrives as a message:
`[from agent <name>] …`.

Use one for work that does not need your context: a test run, a survey of a codebase, a fix in
another workspace. Keep its conclusion, not its transcript.

`ask {to: "<workspace>"}` is the other shape: that workspace's OWN session, which remembers
everything it has done before. A teammate, not an agent.

## Where an agent works

Every agent gets its OWN copy of the workspace (`<ws>-eph-<hex>`), with the same packages — that is
the default, so two agents changing files at once cannot trip over each other and a refactor that
goes wrong is thrown away with the copy. Pass `shared: true` only for a read-only or tiny task in
your own workspace.

It commits on a branch named after itself and pushes (or opens a pull request), then reports with
the branch or the pull. Its copy is deleted once it reports DONE or DONE_WITH_CONCERNS; a BLOCKED
or NEEDS_CONTEXT agent keeps it until you answer or `ask_close {name}`.

    ask {to: "agent", name: "upgrade", task: "Upgrade to Svelte 5 and make the tests pass. Answer with what broke and the branch."}
    ask {to: "agent", name: "audit", shared: true, task: "List every route with no auth check."}
