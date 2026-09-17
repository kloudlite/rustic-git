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

Example:

    ask {to: "agent", name: "audit", task: "In workspace api, list every route with no auth check. Answer with the list and nothing else."}
