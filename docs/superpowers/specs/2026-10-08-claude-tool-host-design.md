# Claude tool host: pi's tools inside the Agent SDK loop, with model switching

Status: design, approved in conversation 2026-10-08. Supersedes the tool and history parts of
`2026-10-05-claude-code-bench-design.md` (Claude Code's built-in tools, `claudeSessionId` resume,
keep-separate families).

## Why

Claude models in the harness run only through the Claude Agent SDK, authenticated by Claude Code's
login (ruling 2026-10-05), with one long-lived `claude` child per session ("it should be fast",
"it should not call for every message"). Today that child runs Claude Code's own built-in tools, so
a Claude session behaves differently from every other model: different tool names, no codemode, no
permission gate, no shared transcript. The owner's rulings:

- "I want all tools run in our process. and I want to provide all tools we are providing to pisdk
  including code mode."
- System prompt: pi's `session.systemPrompt`.
- "entire experience for end user should not differ".
- "we need to provide a way to switch. without loosing transactions."
- "I don't want to use claude code for non claude models" — Claude Code is the loop and connection
  for Claude models only; one loop for every provider is rejected.

## Shape

Claude Code is reduced to the model loop and the connection. Everything else is pi's.

```
TUI ──► ClaudeSession (claude.ts) ──► query() ── claude child ── Anthropic
             │   ▲                        │
             │   └── events ◄─────────────┘
             │                            │ tool call (in-process MCP)
             ▼                            ▼
      pi SessionManager file     claude-tools.ts ──► pi tools (same objects pi runs)
      (the one transcript)       beforeToolCall / execute / afterToolCall
```

### 1. Build pi's session first, harvest its tools and prompt

For a Claude model `createSession` (`harness/packages/agent/src/index.ts`) builds the pi
`AgentSession` exactly as for any other model: same `customTools` from the registry, same
extensions, codemode in `mode: "only"`, same `SessionManager`. The Claude session then takes:

- `session.agent.state.tools`: the active tool objects, codemode included.
- `session.systemPrompt`: pi's assembled prompt, passed as `systemPrompt: <string>` (not the
  `claude_code` preset).
- `session.sessionManager`: the transcript record (section 4).

pi's own loop is never started for a Claude model; the pi session is a tool and record container.

### 2. `claude-tools.ts`: the in-process MCP bridge (~80 lines)

`createSdkMcpServer({ name: "kl", tools })` with one SDK tool per pi tool, same name, same JSON
schema. A call:

1. strips the `mcp__kl__` prefix;
2. runs pi's `beforeToolCall` (the permission gate; a denial returns `{ isError: true }` with the
   gate's reason);
3. emits `tool_execution_start`, calls `tool.execute(id, params, signal, onUpdate)` with
   `onUpdate` emitting `tool_execution_update`;
4. runs `afterToolCall`, emits `tool_execution_end`, returns the result as MCP content. A throw is
   `{ isError: true }` with the message.

`signal` is the session's per-turn `AbortController` signal, combined with MCP `extra.signal`
(`protocol.d.ts:177`; the handler's `extra` is typed `unknown`, narrowed at the call).

`claude.ts` options change to: `tools: []` (no Claude Code built-ins), `mcpServers: { kl }`,
`systemPrompt: <pi's string>`, `permissionMode: "bypassPermissions"` (Claude Code never asks; our
gate does). Tool events come from the bridge, not from parsing `tool_use` blocks, so the TUI sees
exactly what pi emits.

### 3. Parity fixes

- **Steers**: held in our queue and pushed into the input stream when the bridge returns a tool
  result or the turn ends, so `clearQueue` and editing a queued message work. Today they are pushed
  at once (`claude.ts` `steer`) and cannot be recalled.
- **Tokens**: `totalTokens` = input + output + cacheRead + cacheWrite, adding
  `cache_creation_input_tokens` (pi's formula, `pi-ai anthropic-messages.js:648`).
- **Thinking levels**: `off` = `thinking: { type: "disabled" }`; `low`, `medium`, `high`, `xhigh`,
  `max` map 1:1 to `effort` (SDK `EffortLevel` has `xhigh`); only `minimal` falls back to `low`.
  `effortFor` and its test change accordingly.
- **Auto-compaction toggle**: `q.applyFlagSettings({ autoCompactEnabled })` (`sdk.d.ts:9283`).
- **Events**: `system/status: "compacting"` = `compaction_start`; `compact_boundary` =
  `compaction_end`; `api_retry` = `auto_retry_start`, ended by the next `message_start` (success)
  or an error `result` (failure).
- **Not matched, by design**: when compaction triggers, what its summary says, and the retry
  policy. Those belong to Claude Code.

### 4. One transcript, switching both ways

pi's session file is the only record. The TUI always renders pi's messages.

- **While on Claude**: each finished message (user, assistant, tool result) is appended to pi's
  record with `sessionManager.appendMessage` (`session-manager.d.ts:263`) as it lands, so pi's file
  is current at every turn boundary and a switch never loses a turn.
- **Starting a Claude session** (switch from pi, reopen, or new key): `claude-history.ts` converts
  pi's messages into Claude Code transcript entries and the query resumes a fresh session id from
  them through `Options.sessionStore` (`sdk.d.ts:1830`): `load(key)` returns those entries for that
  id (called once, before the child spawns); `append` is a no-op. No `claudeSessionId` is stored;
  `SessionMeta.claudeSessionId` and the `store` option go.
- **Switching Claude to pi**: nothing to convert; the pi session continues on the new model from its
  own file.
- **Tool names** are the same on both sides (`mcp__kl__<name>` in Claude history = `<name>` in pi),
  because every tool is ours.
- **Thinking** from non-Claude models carries no Anthropic signature and is dropped on conversion;
  Claude's own thinking blocks keep their signature. Text, tool calls and tool results carry over.
- `sessionStore` cannot be combined with `persistSession: false`; we leave persistence at its
  default and ignore Claude Code's own files.

Entry shape, verified by spike 2026-10-08 (synthetic history incl. a tool call resumed on
`claude-haiku-4-5`; the model quoted both the earlier user text and the tool result; a second prompt
on the same query worked; one child, one `load`). Every entry: `parentUuid` (null first, then the
previous `uuid`), `isSidechain: false`, `userType: "external"`, `entrypoint: "sdk-cli"`, `cwd`,
`sessionId`, `version`, `gitBranch`, `type`, `message`, `uuid`, `timestamp` (ISO).

- user text: `message: { role: "user", content: <string> }`, plus `promptId`.
- assistant: `message: { model, id: "msg_…", type: "message", role: "assistant", content:
  [text | thinking | tool_use…], stop_reason, stop_sequence: null, usage: { input_tokens,
  output_tokens } }`.
- tool result: `message: { role: "user", content: [{ type: "tool_result", tool_use_id, content,
  is_error }] }`, plus `sourceToolAssistantUUID` (the assistant entry holding the `tool_use`).

Claude Code's own extra entry types (`attachment`, `queue-operation`, `last-prompt`, `mode`,
`cost-state`) are never produced by us and ignored if seen.

Risk: the entry format is Claude Code's undocumented JSONL and `SessionStore` is `@alpha`. The SDK
stays pinned exactly (`0.3.293`); the contract test below fails on an upgrade that breaks it.

### 5. Abort, errors, child death

- `abort()` calls `q.interrupt()` and aborts the turn's `AbortController` (fresh each turn), whose
  signal reaches every running `tool.execute`.
- Tool throw or gate denial = MCP `{ isError: true }`; the model sees it, as under pi.
- Child death: the turn ends with `stopReason: "error"`; the next prompt starts a new query resumed
  from pi's record (section 4), so nothing is lost.

## Files

- `harness/packages/agent/src/claude-tools.ts` (new): the MCP bridge.
- `harness/packages/agent/src/claude-history.ts` (new, ~100 lines): pi messages to Claude entries,
  Claude SDK messages to pi messages.
- `harness/packages/agent/src/claude.ts`: options, bridge events, steer queue, token total,
  thinking map, compaction/retry events, record append, `sessionStore` resume; drop `store`.
- `harness/packages/agent/src/index.ts`: build the pi session for every model; for anthropic wrap
  it in `createClaudeSession`; drop `claudeSessionId` from `SessionMeta`. Family change recreates
  the session object for the key, both reading the same pi file.

## Tests

- `claude-tools.test.ts`: a call runs before/execute/after in order and emits start/update/end; a
  gate denial and a throw both return `isError`; abort reaches `execute`'s signal; prefix stripped.
- `claude-history.test.ts`: pi messages (text, thinking with and without signature, tool call,
  tool result) to entries and back loses nothing but unsigned thinking; `parentUuid` chain and
  `sourceToolAssistantUUID` are right.
- `claude.test.ts` (fake query): options carry `tools: []`, `mcpServers.kl`, pi's prompt string,
  `sessionStore` with the converted history; steer held until tool result; `clearQueue` drops it;
  token total includes cache write; thinking map; compaction and retry events; each finished
  message appended to the record.
- Live smoke (laptop, scratch script, deleted after): two turns on a pi model incl. a tool call,
  switch to `claude-haiku-4-5`, ask about turn 1 (answer must quote it), one `claude` child across
  two prompts, `sleep 30` then Esc stops the tool, a permission card in plan mode, switch back to pi
  and continue.
