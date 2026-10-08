# Claude tool host Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Claude models run pi's own tools (codemode included), pi's system prompt and pi's transcript inside the long-lived Agent SDK loop, so a session switches between Claude and pi models without losing a turn.

**Architecture:** `createSession` builds the pi `AgentSession` for every model. For an anthropic model it wraps that session in `createClaudeSession`, which never starts pi's loop: it hands pi's declared tools to the `claude` child through an in-process MCP server (`claude-tools.ts`), passes `session.systemPrompt` as the system prompt, records every finished message into pi's `SessionManager` and `agent.state.messages`, and on every query start resumes Claude Code from pi's record converted to Claude transcript entries (`claude-history.ts`) via `Options.sessionStore`.

**Tech Stack:** Bun, TypeScript, `@anthropic-ai/claude-agent-sdk` 0.3.293, `@modelcontextprotocol/sdk` 1.32.1, `@earendil-works/pi-coding-agent` / `pi-ai` 1.0.4.

**Spec:** `docs/superpowers/specs/2026-10-08-claude-tool-host-design.md`

## Global Constraints

- Work in `/Volumes/kdisk/rustic-git-wt/master` on the laptop. Never on the `dev` pod.
- SDK stays pinned exactly: `"@anthropic-ai/claude-agent-sdk": "0.3.293"`.
- Add `"@modelcontextprotocol/sdk": "1.32.1"` (exact) to `harness/packages/agent/package.json`.
- pi stays at `1.0.4`; anything reading a pi private field carries a `ponytail:` comment naming the pin.
- Claude Code is the loop and connection for Claude models only. Non-Claude models keep pi's own loop, untouched.
- Claude Code built-in tools are off: `tools: []`. Every tool the model sees is ours, served as `mcp__kl__<name>`.
- `permissionMode: "bypassPermissions"` stays: Claude Code never asks; our `beforeToolCall` gate does.
- Test command per file: `cd harness && bun test packages/agent/src/<file>`. Whole gate: `cd harness && bun run check`. One `Transcript` test failure is known and pre-existing (uncommitted `Transcript.tsx`); anything else failing is yours.
- Never touch `harness/apps/tui/src/components/Transcript.tsx` (uncommitted, not ours).
- Commits: imperative sentence case subject, no tool attribution, no `Co-Authored-By`. Stage explicit paths only (`git add <path>`), never `-a`/`.`.
- House style: comments explain WHY; file header `/** ... */` holds design context.

## Review Focus

1. A `bash` call that runs longer than Claude Code's default MCP tool timeout must still finish and return its output, not be cut off by Claude Code (per-server `timeout: 86_400_000`, Task 3 test asserts the option).
2. Tool arguments are not schema-validated before `execute` (pi validates; the bridge only runs `prepareArguments`). A malformed call must come back as an `isError` result the model can read, never crash the session (Task 2 test: execute throws, result is `isError`).
3. Two parallel tool calls in one assistant message each get their own `toolCallId` and the same recorded assistant message in the gate context (Task 3 test).
4. The `claude` child dying mid-tool must abort the running `execute` and the turn must end with `stopReason: "error"`; the next prompt resumes from pi's record (Task 3 test).
5. A tool call (or a codemode nested call) that reaches the bridge before its assistant message has been recorded waits for it, and falls back to the last assistant message after 5 s instead of hanging (Task 3 test with a held `message_stop`).

## Rulings (decided while planning, against or beyond the spec text)

- **Low-level MCP `Server`, not `createSdkMcpServer`.** The SDK's `tool()` takes only zod schemas; pi tools carry JSON Schema. `Server` from `@modelcontextprotocol/sdk/server/index.js` with `ListTools`/`CallTool` handlers serves pi's `parameters` verbatim. Same behaviour as the spec, different constructor. Passed as `{ type: "sdk", name: "kl", instance: server }`.
- **Declared tools come from a private pi field.** In codemode `only` mode pi keeps every tool in `agent.state.tools` and hides the direct ones per request through `session._hiddenDeclarations`. The Claude session declares `state.tools` minus that set. `ponytail:` comment: private field, pi pinned at 1.0.4, a pin bump re-checks it; the Task 5 test fails if it moves.
- **Record into both `sessionManager` and `agent.state.messages`.** Codemode's nested calls (`_executeNestedToolCall`) look up the issuing assistant message in `agent.state.messages`; without it they fail "No assistant message issued this call".
- **`assistantFor(toolCallId)` wait.** The MCP call can overtake our stream consumer, so the bridge waits for the assistant message holding that `tool_use` id to be recorded (5 s cap, then the last recorded assistant message).
- **Nested events are forwarded.** pi emits codemode's nested `tool_execution_*` (with `parentToolCallId`) on the pi session's own subscribers; the Claude session subscribes and re-emits those.
- **History is one-way code.** `claude-history.ts` converts pi messages to Claude entries only; the Claude-to-pi direction is the stream parser in `claude.ts`, which now builds full pi `AssistantMessage`s (with `toolCall` blocks and thinking signatures). The spec's "to entries and back" test becomes a forward-shape test plus the live smoke round trip.
- **Not matched (minor):** pi records `nestedCalls` details on codemode's tool result inside its own loop; the bridge returns `details` as `execute` gives them and nothing more. Redacted thinking is dropped on conversion.
- **Entry `version`:** a constant `"2.1.291"` (a Claude Code version seen in real transcripts); load does not validate it, the live smoke confirms.

## File Structure

- Create `harness/packages/agent/src/claude-history.ts`: pi messages to Claude Code transcript entries. Pure.
- Create `harness/packages/agent/src/claude-history.test.ts`.
- Create `harness/packages/agent/src/claude-tools.ts`: the MCP server bridging Claude Code tool calls to pi tool objects, and `declaredTools(session)`.
- Create `harness/packages/agent/src/claude-tools.test.ts`.
- Modify `harness/packages/agent/src/claude.ts`: takes the pi session, wires the bridge, records messages, resumes through `sessionStore`, parity fixes.
- Modify `harness/packages/agent/src/claude.test.ts`: fake pi host, rewritten option/tool tests, new parity tests.
- Modify `harness/packages/agent/src/index.ts`: pi session for every model; anthropic wraps it; drop `claudeSessionId`.
- Modify `harness/apps/tui/src/app.tsx:1189-1190`: comment only.
- Modify `harness/packages/agent/package.json`: one dependency.

---

### Task 1: pi messages to Claude transcript entries

**Files:**
- Create: `harness/packages/agent/src/claude-history.ts`
- Test: `harness/packages/agent/src/claude-history.test.ts`

**Interfaces:**
- Consumes: `convertToLlm` from `@earendil-works/pi-coding-agent` (turns pi's extra roles into `user` messages, passes `system`/`user`/`assistant`/`toolResult` through).
- Produces: `export const TOOL_PREFIX = "mcp__kl__"`; `export type ClaudeEntry = { type: string; uuid: string; [k: string]: unknown }`; `export function toClaudeEntries(messages: any[], o: { sessionId: string; cwd: string; model: string }): ClaudeEntry[]`.

- [ ] **Step 1: Write the failing test**

`harness/packages/agent/src/claude-history.test.ts`:

```ts
import { expect, test } from "bun:test";
import { toClaudeEntries } from "./claude-history.ts";

const o = { sessionId: "s-1", cwd: "/w", model: "claude-haiku-4-5" };
const usage = { input: 3, output: 4, cacheRead: 0, cacheWrite: 0, totalTokens: 7, cost: {} };

test("text, signed thinking, tool call and tool result become chained entries", () => {
  const e = toClaudeEntries(
    [
      { role: "system", content: "", sections: {}, timestamp: 1 },
      { role: "user", content: [{ type: "text", text: "list files" }], timestamp: 2 },
      {
        role: "assistant",
        content: [
          { type: "thinking", thinking: "hm", thinkingSignature: "sig" },
          { type: "thinking", thinking: "from gpt" },
          { type: "text", text: "ok" },
          { type: "toolCall", id: "tu1", name: "bash", arguments: { command: "ls" } },
        ],
        api: "openai-responses", provider: "openai", model: "gpt-5", usage, stopReason: "toolUse", timestamp: 3,
      },
      { role: "toolResult", toolCallId: "tu1", toolName: "bash", content: [{ type: "text", text: "a.txt" }], isError: false, timestamp: 4 },
      { role: "assistant", content: [{ type: "text", text: "a.txt" }], api: "x", provider: "openai", model: "gpt-5", usage, stopReason: "stop", timestamp: 5 },
    ],
    o,
  );
  expect(e.map((x) => x.type)).toEqual(["user", "assistant", "user", "assistant"]);
  expect(e[0].parentUuid).toBeNull();
  for (let i = 1; i < e.length; i++) expect(e[i].parentUuid).toBe(e[i - 1].uuid);
  for (const x of e) {
    expect(x).toMatchObject({ isSidechain: false, userType: "external", entrypoint: "sdk-cli", cwd: "/w", sessionId: "s-1" });
    expect(typeof x.timestamp).toBe("string");
  }
  expect(e[0].message).toEqual({ role: "user", content: "list files" });
  expect(typeof e[0].promptId).toBe("string");
  const a = e[1].message as any;
  expect(a).toMatchObject({ role: "assistant", type: "message", stop_reason: "tool_use", stop_sequence: null, usage: { input_tokens: 3, output_tokens: 4 } });
  expect(a.id).toStartWith("msg_");
  // unsigned thinking (another provider's) is dropped; the signed block keeps its signature
  expect(a.content).toEqual([
    { type: "thinking", thinking: "hm", signature: "sig" },
    { type: "text", text: "ok" },
    { type: "tool_use", id: "tu1", name: "mcp__kl__bash", input: { command: "ls" } },
  ]);
  expect(e[2].message).toEqual({
    role: "user",
    content: [{ type: "tool_result", tool_use_id: "tu1", content: [{ type: "text", text: "a.txt" }], is_error: false }],
  });
  expect(e[2].sourceToolAssistantUUID).toBe(e[1].uuid);
  expect((e[3].message as any).stop_reason).toBe("end_turn");
});

test("images carry over; empty assistant messages are skipped", () => {
  const e = toClaudeEntries(
    [
      { role: "user", content: [{ type: "image", data: "AAA", mimeType: "image/png" }, { type: "text", text: "see" }], timestamp: 1 },
      { role: "assistant", content: [], api: "x", provider: "anthropic", model: "m", usage, stopReason: "aborted", timestamp: 2 },
    ],
    o,
  );
  expect(e.length).toBe(1);
  expect((e[0].message as any).content).toEqual([
    { type: "image", source: { type: "base64", media_type: "image/png", data: "AAA" } },
    { type: "text", text: "see" },
  ]);
});

test("a tool call left without a result gets an error result, so the API accepts the history", () => {
  const e = toClaudeEntries(
    [
      { role: "user", content: "go", timestamp: 1 },
      { role: "assistant", content: [{ type: "toolCall", id: "tu9", name: "bash", arguments: {} }], api: "x", provider: "anthropic", model: "m", usage, stopReason: "toolUse", timestamp: 2 },
      { role: "user", content: "next", timestamp: 3 },
    ],
    o,
  );
  expect(e.map((x) => x.type)).toEqual(["user", "assistant", "user", "user"]);
  expect((e[2].message as any).content[0]).toMatchObject({ type: "tool_result", tool_use_id: "tu9", is_error: true });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd harness && bun test packages/agent/src/claude-history.test.ts`
Expected: FAIL, `Cannot find module './claude-history.ts'`.

- [ ] **Step 3: Write the implementation**

`harness/packages/agent/src/claude-history.ts`:

```ts
/**
 * pi's transcript as Claude Code transcript entries.
 *
 * pi's session file is the one record of a conversation (spec
 * 2026-10-08-claude-tool-host-design.md, section 4). When a Claude query
 * starts - a switch from a pi model, a reopen, a dead child - Claude Code is
 * resumed from these entries through `Options.sessionStore.load`, so the model
 * sees every earlier turn whichever provider produced it.
 *
 * Entry shape verified by spike 2026-10-08 on claude-haiku-4-5. The format is
 * Claude Code's undocumented JSONL and `SessionStore` is `@alpha`: the SDK is
 * pinned exactly and the live smoke re-checks it on any bump.
 *
 * Dropped on purpose: thinking without an Anthropic signature (another
 * provider's, the API rejects it) and redacted thinking; pi's `system`
 * messages (Claude gets pi's prompt as its system prompt instead).
 */
import { randomUUID } from "node:crypto";
import { convertToLlm } from "@earendil-works/pi-coding-agent";

export const TOOL_PREFIX = "mcp__kl__";
/** Claude Code version stamped on entries; load does not check it. */
const ENTRY_VERSION = "2.1.291";

export type ClaudeEntry = { type: string; uuid: string; [k: string]: unknown };

const blocks = (content: any) =>
  typeof content === "string"
    ? content
    : (content ?? []).flatMap((b: any) =>
        b.type === "text"
          ? [{ type: "text", text: b.text }]
          : b.type === "image"
            ? [{ type: "image", source: { type: "base64", media_type: b.mimeType, data: b.data } }]
            : [],
      );

// a lone text block goes as a plain string, the shape the spike verified
const userContent = (content: any) => {
  const b = blocks(content);
  return Array.isArray(b) && b.length === 1 && b[0].type === "text" ? b[0].text : b;
};

const stopReason = (r: string) => (r === "toolUse" ? "tool_use" : r === "length" ? "max_tokens" : "end_turn");

export function toClaudeEntries(messages: any[], o: { sessionId: string; cwd: string; model: string }): ClaudeEntry[] {
  const out: ClaudeEntry[] = [];
  let parent: string | null = null;
  // tool_use id -> uuid of the assistant entry that issued it, until answered
  const open = new Map<string, string>();
  const push = (type: string, message: unknown, ts: number | undefined, extra: object = {}) => {
    const uuid = randomUUID();
    out.push({
      parentUuid: parent,
      isSidechain: false,
      userType: "external",
      entrypoint: "sdk-cli",
      cwd: o.cwd,
      sessionId: o.sessionId,
      version: ENTRY_VERSION,
      gitBranch: "",
      type,
      message,
      uuid,
      timestamp: new Date(ts ?? Date.now()).toISOString(),
      ...extra,
    });
    parent = uuid;
    return uuid;
  };
  const result = (id: string, content: unknown, isError: boolean, ts?: number) => {
    push("user", { role: "user", content: [{ type: "tool_result", tool_use_id: id, content, is_error: isError }] }, ts, {
      sourceToolAssistantUUID: open.get(id),
    });
    open.delete(id);
  };
  // the API refuses a tool_use with no tool_result after it (an aborted turn)
  const closeOpen = () => {
    for (const id of [...open.keys()]) result(id, [{ type: "text", text: "Tool call was interrupted." }], true);
  };
  for (const m of convertToLlm(messages) as any[]) {
    if (m.role === "user") {
      closeOpen();
      push("user", { role: "user", content: userContent(m.content) }, m.timestamp, { promptId: randomUUID() });
    } else if (m.role === "assistant") {
      closeOpen();
      const content = (m.content ?? []).flatMap((b: any) =>
        b.type === "text"
          ? [{ type: "text", text: b.text }]
          : b.type === "thinking" && b.thinkingSignature && !b.redacted
            ? [{ type: "thinking", thinking: b.thinking, signature: b.thinkingSignature }]
            : b.type === "toolCall"
              ? [{ type: "tool_use", id: b.id, name: TOOL_PREFIX + b.name, input: b.arguments ?? {} }]
              : [],
      );
      if (!content.length) continue;
      const uuid = push(
        "assistant",
        {
          model: m.model ?? o.model,
          id: `msg_${randomUUID().replaceAll("-", "")}`,
          type: "message",
          role: "assistant",
          content,
          stop_reason: stopReason(m.stopReason),
          stop_sequence: null,
          usage: { input_tokens: m.usage?.input ?? 0, output_tokens: m.usage?.output ?? 0 },
        },
        m.timestamp,
      );
      for (const b of content) if (b.type === "tool_use") open.set(b.id, uuid);
    } else if (m.role === "toolResult" && open.has(m.toolCallId)) {
      result(m.toolCallId, blocks(m.content), !!m.isError, m.timestamp);
    }
  }
  return out;
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd harness && bun test packages/agent/src/claude-history.test.ts`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
cd /Volumes/kdisk/rustic-git-wt/master
git add harness/packages/agent/src/claude-history.ts harness/packages/agent/src/claude-history.test.ts
git commit -m "Convert pi transcripts to Claude Code transcript entries"
```

---

### Task 2: the in-process MCP bridge

**Files:**
- Create: `harness/packages/agent/src/claude-tools.ts`
- Test: `harness/packages/agent/src/claude-tools.test.ts`
- Modify: `harness/packages/agent/package.json` (dependencies)

**Interfaces:**
- Consumes: `TOOL_PREFIX` from `./claude-history.ts`.
- Produces:
  ```ts
  export type ToolHost = {
    tools(): any[];                                  // pi AgentTool objects, read per call
    agent: { beforeToolCall?: (ctx: any, signal?: AbortSignal) => Promise<any>;
             afterToolCall?: (ctx: any, signal?: AbortSignal) => Promise<any>;
             state: { messages: any[]; tools: any[] } };
    assistantFor(toolCallId: string): Promise<any>;  // the recorded assistant message
    signal(): AbortSignal;                           // the current turn's
    emit(event: any): void;                          // tool_execution_* to the TUI
    onResult(message: any): void;                    // pi ToolResultMessage, to record
  };
  export function createToolServer(host: ToolHost): Server;
  export function declaredTools(session: { agent: { state: { tools: any[] } } }): any[];
  ```

- [ ] **Step 1: Add the dependency**

In `harness/packages/agent/package.json` `dependencies`, after `"@kloudlite-tui/tools": "workspace:*"` add `"@modelcontextprotocol/sdk": "1.32.1"` (comma on the previous line). Run `cd harness && bun install`. Expected: lockfile updates, no new download (already in `node_modules/.bun`).

- [ ] **Step 2: Write the failing test**

`harness/packages/agent/src/claude-tools.test.ts`:

```ts
import { expect, test } from "bun:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { createToolServer, declaredTools, type ToolHost } from "./claude-tools.ts";

const assistant = { role: "assistant", content: [{ type: "toolCall", id: "tu1", name: "bash", arguments: {} }] };

function host(tool: any, over: Partial<ToolHost> = {}) {
  const events: any[] = [];
  const results: any[] = [];
  const order: string[] = [];
  const turn = new AbortController();
  const h: ToolHost = {
    tools: () => [tool],
    agent: {
      state: { messages: [assistant], tools: [tool] },
      beforeToolCall: async (ctx) => {
        order.push(`before:${ctx.toolCall.id}:${ctx.assistantMessage === assistant}`);
        return undefined;
      },
      afterToolCall: async () => {
        order.push("after");
        return undefined;
      },
    },
    assistantFor: async () => assistant,
    signal: () => turn.signal,
    emit: (e) => events.push(e),
    onResult: (m) => results.push(m),
    ...over,
  };
  return { h, events, results, order, turn };
}

async function connect(h: ToolHost) {
  const [a, b] = InMemoryTransport.createLinkedPair();
  await createToolServer(h).connect(a);
  const client = new Client({ name: "t", version: "1" });
  await client.connect(b);
  return client;
}

const bash = (execute: any) => ({
  name: "bash",
  label: "bash",
  description: "run",
  parameters: { type: "object", properties: { command: { type: "string" } } },
  execute,
});

test("list serves pi's JSON schema and asks Claude Code to load it up front", async () => {
  const { h } = host(bash(async () => ({ content: [] })));
  const c = await connect(h);
  const { tools } = await c.listTools();
  expect(tools).toEqual([
    {
      name: "bash",
      description: "run",
      inputSchema: { type: "object", properties: { command: { type: "string" } } },
      _meta: { "anthropic/alwaysLoad": true },
    },
  ]);
});

test("a call runs before, execute, after in order and emits start, update, end", async () => {
  const { h, events, results, order } = host(
    bash(async (_id: string, args: any, _s: AbortSignal, onUpdate: any) => {
      order.push(`execute:${args.command}`);
      onUpdate({ content: [{ type: "text", text: "partial" }] });
      return { content: [{ type: "text", text: "a.txt", textSignature: "x" }], details: { n: 1 } };
    }),
  );
  const c = await connect(h);
  const r = await c.callTool({ name: "bash", arguments: { command: "ls" }, _meta: { "claudecode/toolUseId": "tu1" } });
  expect(order).toEqual(["before:tu1:true", "execute:ls", "after"]);
  expect(events.map((e) => e.type)).toEqual(["tool_execution_start", "tool_execution_update", "tool_execution_end"]);
  expect(events[0]).toMatchObject({ toolCallId: "tu1", toolName: "bash", args: { command: "ls" } });
  expect(events[2]).toMatchObject({ toolCallId: "tu1", isError: false, result: { content: [{ type: "text", text: "a.txt" }] } });
  expect(r).toEqual({ content: [{ type: "text", text: "a.txt" }], isError: false });
  expect(results[0]).toMatchObject({ role: "toolResult", toolCallId: "tu1", toolName: "bash", isError: false, details: { n: 1 } });
});

test("the prefixed name is stripped", async () => {
  const { h, events } = host(bash(async () => ({ content: [] })));
  const c = await connect(h);
  await c.callTool({ name: "mcp__kl__bash", arguments: {} });
  expect(events[0].toolName).toBe("bash");
});

test("a gate denial and a throw both come back as isError with the reason", async () => {
  const denied = host(bash(async () => ({ content: [] })), {
    agent: { state: { messages: [], tools: [] }, beforeToolCall: async () => ({ block: true, reason: "The user rejected this tool call." }) },
  });
  const c1 = await connect(denied.h);
  expect(await c1.callTool({ name: "bash", arguments: {} })).toEqual({
    content: [{ type: "text", text: "The user rejected this tool call." }],
    isError: true,
  });
  const thrown = host(bash(async () => { throw new Error("boom"); }));
  const c2 = await connect(thrown.h);
  expect(await c2.callTool({ name: "bash", arguments: {} })).toEqual({ content: [{ type: "text", text: "boom" }], isError: true });
  expect(thrown.results[0].isError).toBe(true);
});

test("afterToolCall overrides content and the error flag", async () => {
  const { h } = host(bash(async () => ({ content: [{ type: "text", text: "raw" }] })), {
    agent: { state: { messages: [], tools: [] }, afterToolCall: async () => ({ content: [{ type: "text", text: "redacted" }], isError: true }) },
  });
  const c = await connect(h);
  expect(await c.callTool({ name: "bash", arguments: {} })).toEqual({ content: [{ type: "text", text: "redacted" }], isError: true });
});

test("aborting the turn reaches execute's signal", async () => {
  let seen: AbortSignal | undefined;
  const { h, turn } = host(
    bash((_id: string, _a: any, signal: AbortSignal) => {
      seen = signal;
      return new Promise((_r, reject) => signal.addEventListener("abort", () => reject(new Error("aborted"))));
    }),
  );
  const c = await connect(h);
  const call = c.callTool({ name: "bash", arguments: {} });
  await new Promise((r) => setTimeout(r, 20));
  turn.abort();
  expect(await call).toEqual({ content: [{ type: "text", text: "aborted" }], isError: true });
  expect(seen?.aborted).toBe(true);
});

test("an unknown tool is an error result, not a crash", async () => {
  const { h } = host(bash(async () => ({ content: [] })));
  const c = await connect(h);
  expect(await c.callTool({ name: "nope", arguments: {} })).toEqual({ content: [{ type: "text", text: "Tool nope not found" }], isError: true });
});

test("declaredTools drops what codemode hides from requests", () => {
  const s: any = { agent: { state: { tools: [{ name: "codemode" }, { name: "bash" }] } }, _hiddenDeclarations: new Set(["bash"]) };
  expect(declaredTools(s).map((t) => t.name)).toEqual(["codemode"]);
  expect(declaredTools({ agent: { state: { tools: [{ name: "bash" }] } } }).map((t) => t.name)).toEqual(["bash"]);
});
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cd harness && bun test packages/agent/src/claude-tools.test.ts`
Expected: FAIL, `Cannot find module './claude-tools.ts'`.

- [ ] **Step 4: Write the implementation**

`harness/packages/agent/src/claude-tools.ts`:

```ts
/**
 * pi's tools, served to the `claude` child as an in-process MCP server.
 *
 * Ruling 2026-10-08: "all tools run in our process", codemode included. Claude
 * Code is only the model loop; each tool call comes back here and runs the
 * same pi tool object pi's own loop would run, behind the same
 * `beforeToolCall` (the TUI's permission gate) and `afterToolCall`, emitting
 * the same `tool_execution_*` events.
 *
 * A low-level MCP `Server`, not the SDK's `createSdkMcpServer`: that one takes
 * zod schemas only, and pi tools carry JSON Schema, served here verbatim.
 * `anthropic/alwaysLoad` keeps Claude Code from deferring our tools behind its
 * tool search.
 *
 * ponytail: arguments are not schema-validated (pi's loop validates; this
 * runs `prepareArguments` only). A bad call fails inside `execute` and comes
 * back as an error result the model reads. Add validation if a tool misbehaves
 * on bad input.
 */
import { randomUUID } from "node:crypto";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { TOOL_PREFIX } from "./claude-history.ts";

export type ToolHost = {
  tools(): any[];
  agent: {
    beforeToolCall?: (ctx: any, signal?: AbortSignal) => Promise<any>;
    afterToolCall?: (ctx: any, signal?: AbortSignal) => Promise<any>;
    state: { messages: any[]; tools: any[] };
  };
  assistantFor(toolCallId: string): Promise<any>;
  signal(): AbortSignal;
  emit(event: any): void;
  onResult(message: any): void;
};

/**
 * The tools pi would declare to the model. In codemode `only` mode pi keeps
 * every tool in `state.tools` and strips the direct ones from each request.
 * ponytail: reads pi's private `_hiddenDeclarations` (pi pinned at 1.0.4); a
 * pin bump re-checks it, and `index.test.ts` fails if it moves.
 */
export function declaredTools(session: { agent: { state: { tools: any[] } } }): any[] {
  const hidden: Set<string> = (session as any)._hiddenDeclarations ?? new Set();
  return session.agent.state.tools.filter((t) => !hidden.has(t.name));
}

// pi content is MCP content already, minus pi's own fields
const mcp = (c: any) => (c.type === "image" ? { type: "image", data: c.data, mimeType: c.mimeType } : { type: "text", text: c.text });

async function call(host: ToolHost, name: string, id: string, raw: any, signal: AbortSignal) {
  const done = (content: any[], isError: boolean, details?: unknown) => {
    host.emit({ type: "tool_execution_end", toolCallId: id, toolName: name, result: { content, details }, isError });
    host.onResult({ role: "toolResult", toolCallId: id, toolName: name, content, details, isError, timestamp: Date.now() });
    return { content: content.map(mcp), isError };
  };
  const fail = (e: unknown) => done([{ type: "text", text: e instanceof Error ? e.message : String(e) }], true);
  const tool = host.tools().find((t) => t.name === name);
  if (!tool) {
    host.emit({ type: "tool_execution_start", toolCallId: id, toolName: name, args: raw });
    return fail(`Tool ${name} not found`);
  }
  try {
    const args = tool.prepareArguments?.(raw) ?? raw;
    host.emit({ type: "tool_execution_start", toolCallId: id, toolName: name, args });
    const assistantMessage = await host.assistantFor(id);
    const toolCall = { type: "toolCall", id, name, arguments: args };
    const context = { messages: host.agent.state.messages, tools: host.agent.state.tools };
    const gate = await host.agent.beforeToolCall?.({ assistantMessage, toolCall, args, context }, signal);
    if (gate?.block) return done([{ type: "text", text: gate.reason ?? "Tool call blocked" }], true);
    const r = await tool.execute(id, args, signal, (partialResult: any) =>
      host.emit({ type: "tool_execution_update", toolCallId: id, toolName: name, args, partialResult }),
    );
    let content = r?.content ?? [];
    let details = r?.details;
    let isError = !!r?.isError;
    const after = await host.agent.afterToolCall?.({ assistantMessage, toolCall, args, result: r, isError, context }, signal);
    if (after?.content) content = after.content;
    if (after?.details !== undefined) details = after.details;
    if (after?.isError !== undefined) isError = after.isError;
    return done(content, isError, details);
  } catch (e) {
    return fail(e);
  }
}

export function createToolServer(host: ToolHost): Server {
  const server = new Server({ name: "kl", version: "1" }, { capabilities: { tools: {} } });
  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: host.tools().map((t) => ({
      name: t.name,
      description: t.description,
      inputSchema: t.parameters,
      _meta: { "anthropic/alwaysLoad": true },
    })),
  }));
  server.setRequestHandler(CallToolRequestSchema, async (req, extra) => {
    const name = req.params.name.startsWith(TOOL_PREFIX) ? req.params.name.slice(TOOL_PREFIX.length) : req.params.name;
    const id = String((req.params._meta as any)?.["claudecode/toolUseId"] ?? randomUUID());
    const signal = AbortSignal.any([host.signal(), extra.signal]);
    return call(host, name, id, req.params.arguments ?? {}, signal) as any;
  });
  return server;
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cd harness && bun test packages/agent/src/claude-tools.test.ts`
Expected: PASS, 8 tests. If `listTools` drops `_meta` in the client's parsed output, assert on the server side instead by calling the handler through `client.request({ method: "tools/list" }, ListToolsResultSchema)` (same schema import); do not remove `_meta` from the server.

- [ ] **Step 6: Commit**

```bash
cd /Volumes/kdisk/rustic-git-wt/master
git add harness/packages/agent/package.json harness/bun.lock harness/packages/agent/src/claude-tools.ts harness/packages/agent/src/claude-tools.test.ts
git commit -m "Serve pi tools to Claude Code through an in-process MCP server"
```

(If `bun install` changed no lockfile, drop `harness/bun.lock` from the add.)

---

### Task 3: Claude session runs on pi's session: tools, record, resume

**Files:**
- Modify: `harness/packages/agent/src/claude.ts`
- Test: `harness/packages/agent/src/claude.test.ts`

**Interfaces:**
- Consumes: `toClaudeEntries`, `TOOL_PREFIX` (Task 1); `createToolServer`, `declaredTools`, `ToolHost` (Task 2).
- Produces (used by Task 5):
  ```ts
  export type PiHost = {
    agent: { state: { messages: any[]; tools: any[] }; beforeToolCall?: any; afterToolCall?: any };
    systemPrompt: string;
    sessionManager: { appendMessage(m: any): unknown; buildSessionContext(): { messages: any[] } };
    subscribe(l: (e: any) => void): () => void;
    dispose?(): void;
  };
  export type ClaudeOptions = {
    key: string; model: { id: string }; thinkingLevel?: Level;
    pi: PiHost; cwd?: string; query?: QueryFn; timingLog?: string;
  };
  ```
  `fresh` and `store` are removed (pi's `SessionManager.create` handles `/clear`). The returned session's `agent` IS `pi.agent`, and `messages` returns `pi.agent.state.messages`.

- [ ] **Step 1: Rewrite the test harness and the affected tests**

In `claude.test.ts`:

Replace the `store` helper and `run` (lines 51-61) with a fake pi host:

```ts
function piHost(messages: any[] = []) {
  const recorded: any[] = [];
  const subs = new Set<(e: any) => void>();
  const tools: any[] = [];
  const host = {
    agent: { state: { messages: [...messages], tools } } as any,
    systemPrompt: "PI PROMPT",
    sessionManager: {
      appendMessage: (m: any) => recorded.push(m),
      buildSessionContext: () => ({ messages: [...messages, ...recorded] }),
    },
    subscribe: (l: (e: any) => void) => (subs.add(l), () => subs.delete(l)),
    emitPi: (e: any) => subs.forEach((l) => l(e)),
  };
  return { host, recorded, tools };
}

function run(f: ReturnType<typeof fake>, extra: object = {}) {
  const p = piHost();
  const s = createClaudeSession({ key: "k", model: { id: "claude-haiku-4-5" }, pi: p.host, query: f.query, ...extra });
  const events: any[] = [];
  s.subscribe((e) => events.push(e));
  return { s, events, p };
}
```

Replace the test at lines 84-105 ("the claude session id is saved...") with:

```ts
test("options: no built-in tools, our MCP server, pi's prompt, history through sessionStore", async () => {
  const f = fake((_n, push) => text("x").forEach(push));
  const p = piHost([{ role: "user", content: [{ type: "text", text: "earlier" }], timestamp: 1 }]);
  const s = createClaudeSession({ key: "k", model: { id: "m" }, pi: p.host, query: f.query });
  await s.prompt("a");
  await tick();
  const o = f.calls[0].options;
  expect(o.tools).toEqual([]);
  expect(o.mcpServers.kl).toMatchObject({ type: "sdk", name: "kl" });
  expect(o.mcpServers.kl.timeout).toBe(86_400_000);
  expect(o.systemPrompt).toBe("PI PROMPT");
  expect(o.permissionMode).toBe("bypassPermissions");
  expect(o.settingSources).toEqual([]);
  expect(o.includePartialMessages).toBe(true);
  expect(o.extraArgs["thinking-display"]).toBe("summarized");
  expect(typeof o.resume).toBe("string");
  const entries = await o.sessionStore.load({ projectKey: "x", sessionId: o.resume });
  expect(entries.map((e: any) => e.message.content)).toEqual(["earlier"]);
  expect(await o.sessionStore.load({ projectKey: "x", sessionId: "other" })).toBeNull();
  expect(await o.sessionStore.load({ projectKey: "x", sessionId: o.resume, subpath: "agent-1" })).toBeNull();
  s.dispose();
  // nothing recorded yet: no resume
  const empty = piHost();
  createClaudeSession({ key: "k", model: { id: "m" }, pi: empty.host, query: f.query }).prompt("b");
  expect(f.calls[1].options.resume).toBeUndefined();
});

test("each finished message is recorded in pi's file and pi's state", async () => {
  const f = fake((_n, push) => text("hi").forEach(push));
  const { s, p } = run(f);
  await s.prompt("one");
  await tick();
  expect(p.recorded.map((m) => m.role)).toEqual(["user", "assistant"]);
  expect(p.recorded[0].content).toEqual([{ type: "text", text: "one" }]);
  expect(p.recorded[1]).toMatchObject({ role: "assistant", provider: "anthropic", api: "anthropic-messages", model: "claude-haiku-4-5", stopReason: "stop" });
  expect(p.recorded[1].content.find((b: any) => b.type === "text").text).toBe("hi");
  expect(p.host.agent.state.messages.map((m: any) => m.role)).toEqual(["user", "assistant"]);
  expect(s.messages).toBe(p.host.agent.state.messages);
  expect(s.agent).toBe(p.host.agent);
  s.dispose();
});
```

Replace the tool test at lines 107-126 ("tool_use and tool_result become...") with tests that drive the bridge through the MCP server the session hands to `query`:

```ts
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";

async function mcpOf(options: any) {
  const [a, b] = InMemoryTransport.createLinkedPair();
  await options.mcpServers.kl.instance.connect(a);
  const c = new Client({ name: "t", version: "1" });
  await c.connect(b);
  return c;
}

const toolTurn = (ids: string[], push: (m: any) => void, stop = true) => {
  push({ type: "stream_event", event: { type: "message_start", message: { id: `t${ids[0]}`, usage: { input_tokens: 1 } } } });
  ids.forEach((id, i) => {
    push({ type: "stream_event", event: { type: "content_block_start", index: i, content_block: { type: "tool_use", id, name: "mcp__kl__bash" } } });
    push({ type: "stream_event", event: { type: "content_block_delta", index: i, delta: { type: "input_json_delta", partial_json: '{"command":"ls"}' } } });
    push({ type: "stream_event", event: { type: "content_block_stop", index: i } });
  });
  push({ type: "stream_event", event: { type: "message_delta", delta: { stop_reason: "tool_use" }, usage: { output_tokens: 2 } } });
  if (stop) push({ type: "stream_event", event: { type: "message_stop" } });
};

test("tool_use stays in the message as a toolCall; the bridge runs the pi tool and records the result", async () => {
  let pushOut: (m: any) => void = () => {};
  const f = fake((_n, push) => {
    pushOut = push;
    toolTurn(["tu1"], push);
  });
  const { s, events, p } = run(f);
  const ran: string[] = [];
  p.tools.push({ name: "bash", description: "d", parameters: { type: "object" }, execute: async (id: string) => (ran.push(id), { content: [{ type: "text", text: "a.txt" }] }) });
  await s.prompt("go");
  await tick();
  const c = await mcpOf(f.calls[0].options);
  const r = await c.callTool({ name: "bash", arguments: { command: "ls" }, _meta: { "claudecode/toolUseId": "tu1" } });
  expect(r).toEqual({ content: [{ type: "text", text: "a.txt" }], isError: false });
  expect(ran).toEqual(["tu1"]);
  const assistant = p.recorded.find((m) => m.role === "assistant");
  expect(assistant.stopReason).toBe("toolUse");
  expect(assistant.content).toContainEqual({ type: "toolCall", id: "tu1", name: "bash", arguments: { command: "ls" } });
  expect(p.recorded.at(-1)).toMatchObject({ role: "toolResult", toolCallId: "tu1", isError: false });
  expect(events.filter((e) => e.type.startsWith("tool_execution")).map((e) => e.type)).toEqual(["tool_execution_start", "tool_execution_end"]);
  pushOut({ type: "result", subtype: "success" });
  s.dispose();
});

test("parallel calls each get their id and the same recorded assistant message", async () => {
  const f = fake((_n, push) => toolTurn(["tu1", "tu2"], push));
  const { s, p } = run(f);
  const seen: any[] = [];
  p.tools.push({ name: "bash", description: "d", parameters: { type: "object" }, execute: async () => ({ content: [] }) });
  p.host.agent.beforeToolCall = async (ctx: any) => (seen.push([ctx.toolCall.id, ctx.assistantMessage]), undefined);
  await s.prompt("go");
  await tick();
  const c = await mcpOf(f.calls[0].options);
  await Promise.all(["tu1", "tu2"].map((id) => c.callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": id } })));
  expect(seen.map((x) => x[0]).sort()).toEqual(["tu1", "tu2"]);
  expect(seen[0][1]).toBe(seen[1][1]);
  expect(seen[0][1].content.filter((b: any) => b.type === "toolCall").length).toBe(2);
  s.dispose();
});

test("a call that overtakes its assistant message waits for it to be recorded", async () => {
  let pushOut: (m: any) => void = () => {};
  const f = fake((_n, push) => {
    pushOut = push;
    toolTurn(["tu1"], push, false); // message_stop held back
  });
  const { s, p } = run(f);
  let gateSaw: any;
  p.tools.push({ name: "bash", description: "d", parameters: { type: "object" }, execute: async () => ({ content: [] }) });
  p.host.agent.beforeToolCall = async (ctx: any) => ((gateSaw = ctx.assistantMessage), undefined);
  await s.prompt("go");
  await tick();
  const c = await mcpOf(f.calls[0].options);
  const call = c.callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": "tu1" } });
  await tick();
  expect(gateSaw).toBeUndefined();
  pushOut({ type: "stream_event", event: { type: "message_stop" } });
  await call;
  expect(gateSaw.content).toContainEqual({ type: "toolCall", id: "tu1", name: "bash", arguments: { command: "ls" } });
  expect(p.host.agent.state.messages).toContain(gateSaw);
  s.dispose();
});

test("child death mid-tool aborts execute and ends the turn with an error", async () => {
  let die: () => void = () => {};
  const f = fake((_n, push) => toolTurn(["tu1"], push));
  // a query whose iterator throws when told to: the child died
  const query = (p: any) => {
    const q = f.query(p);
    const it = q[Symbol.asyncIterator]();
    let dead: (e: Error) => void = () => {};
    const death = new Promise<never>((_r, reject) => (dead = reject));
    die = () => dead(new Error("claude exited"));
    return Object.assign(q, {
      [Symbol.asyncIterator]: () => ({ next: () => Promise.race([it.next(), death]) }),
    });
  };
  const { s, events, p } = run({ ...f, query: query as any });
  let signal: AbortSignal | undefined;
  p.tools.push({
    name: "bash", description: "d", parameters: { type: "object" },
    execute: (_id: string, _a: any, sig: AbortSignal) => ((signal = sig), new Promise((_r, rej) => sig.addEventListener("abort", () => rej(new Error("aborted"))))),
  });
  await s.prompt("go");
  await tick();
  const c = await mcpOf(f.calls[0].options);
  void c.callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": "tu1" } });
  await tick();
  die();
  await tick();
  expect(signal?.aborted).toBe(true);
  expect(events.some((e) => e.type === "message_end" && e.message.stopReason === "error")).toBe(true);
  expect(events.at(-1).type).toBe("agent_end");
  // next prompt starts a new query resumed from pi's record
  await s.prompt("again");
  expect(f.calls.length).toBe(2);
  const entries = await f.calls[1].options.sessionStore.load({ projectKey: "x", sessionId: f.calls[1].options.resume });
  expect(entries.some((e: any) => e.type === "assistant")).toBe(true);
  s.dispose();
});

test("codemode's nested tool events from the pi session reach the TUI", async () => {
  const f = fake(() => {});
  const { s, events, p } = run(f);
  (p.host as any).emitPi({ type: "tool_execution_start", toolCallId: "n1", toolName: "bash", args: {}, parentToolCallId: "tu1" });
  (p.host as any).emitPi({ type: "tool_execution_start", toolCallId: "x", toolName: "bash", args: {} });
  (p.host as any).emitPi({ type: "message_end", message: {} });
  expect(events.map((e) => e.toolCallId)).toEqual(["n1"]);
  s.dispose();
});
```

In "each turn appends one timing line" (line 184) nothing changes; `run` already provides a pi host.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd harness && bun test packages/agent/src/claude.test.ts`
Expected: FAIL; first failure in "options: no built-in tools..." (`o.tools` undefined).

- [ ] **Step 3: Implement in `claude.ts`**

Exact edits:

1. Header comment (lines 18-31): replace the "Tools are Claude Code's built-ins..." and "History: Claude Code holds the transcript..." paragraphs with:

   ```
    * Tools, prompt and transcript are pi's (spec 2026-10-08-claude-tool-host).
    * The pi AgentSession is built as for any model but its loop never runs:
    * its declared tools are served to the child over in-process MCP
    * (`claude-tools.ts`), `session.systemPrompt` is the system prompt, and
    * every finished message is recorded into pi's file and `agent.state`.
    * Each query start resumes Claude Code from pi's record
    * (`claude-history.ts` through `sessionStore`), so switching to or from a
    * pi model never loses a turn. Claude Code's built-in tools are off.
   ```

2. Imports: add
   ```ts
   import { randomUUID } from "node:crypto";
   import { TOOL_PREFIX, toClaudeEntries } from "./claude-history.ts";
   import { createToolServer, declaredTools } from "./claude-tools.ts";
   ```

3. Replace `ClaudeOptions` (lines 125-137) with `PiHost` + `ClaudeOptions` exactly as in **Interfaces** above (keep the `query` and `timingLog` doc comments).

4. Delete `pi`, `piArgs`, `textOf` (lines 187-206).

5. In `createClaudeSession`:
   - Remove `sessionId` / `opts.fresh` / `opts.store` lines (217-218) and the `tools` map (238).
   - Add after `const emit`:
     ```ts
     const piSession = opts.pi;
     const record = (m: any) => {
       piSession.sessionManager.appendMessage(m);
       piSession.agent.state.messages = [...piSession.agent.state.messages, m];
     };
     let turn = new AbortController();
     // tool_use id -> the recorded assistant message that issued it (this turn)
     const issued = new Map<string, any>();
     const waiting = new Map<string, (m: any) => void>();
     const lastAssistant = () => piSession.agent.state.messages.findLast((m: any) => m.role === "assistant");
     /**
      * The MCP call can overtake our read of the stream, and the gate and
      * codemode's nested calls need the issuing message recorded first.
      * ponytail: 5 s cap then the last assistant message; a stream that slow
      * has bigger problems.
      */
     const assistantFor = (id: string) =>
       issued.has(id)
         ? Promise.resolve(issued.get(id))
         : new Promise<any>((resolve) => {
             const t = setTimeout(() => (waiting.delete(id), resolve(lastAssistant())), 5_000);
             waiting.set(id, (m) => (clearTimeout(t), resolve(m)));
           });
     // one server per query: an MCP Server binds one transport, and each start() spawns a new child
     const toolServer = () => createToolServer({
       tools: () => declaredTools(piSession),
       agent: piSession.agent,
       assistantFor,
       signal: () => turn.signal,
       emit: (e) => {
         if (e.type === "tool_execution_start" && timing) timing.tool ??= since();
         emit(e);
       },
       onResult: (m) => record(m),
     });
     // codemode's nested calls are emitted on the pi session itself
     const unsubPi = piSession.subscribe((e: any) => {
       if (e?.parentToolCallId && String(e.type).startsWith("tool_execution_")) emit(e);
     });
     ```
     `timing`/`since` are declared below this block today; move the `let timing` and `const since` lines above it.
   - `newMessage`: build a full pi `AssistantMessage`:
     ```ts
     const newMessage = (extra: object = {}) => ({
       role: "assistant",
       content: [] as any[],
       api: "anthropic-messages",
       provider: "anthropic",
       model,
       usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
       stopReason: "stop",
       timestamp: stamp(),
       ...extra,
     });
     ```
   - `errorMessage`: after emitting `message_end`, `record(msg)`.
   - `endCur`:
     ```ts
     function endCur() {
       if (!cur) return;
       const msg = cur.msg;
       cur = undefined;
       record(msg);
       emit({ type: "message_end", message: msg });
       for (const b of msg.content)
         if (b.type === "toolCall") {
           issued.set(b.id, msg);
           waiting.get(b.id)?.(msg);
           waiting.delete(b.id);
         }
     }
     ```
   - `finish`: after `running = false`, add `issued.clear();`.
   - Delete `toolStart`.
   - `onStream`:
     - `message_start`: `cur = { msg: newMessage(), parts: new Map(), id: ev.message?.id, input: ev.message?.usage?.input_tokens ?? 0 };` (keep the rest of the case for now; Task 4 changes tokens and steering).
     - `content_block_start` thinking: `const part = { type: "thinking", thinking: "", thinkingSignature: "" };`
     - `content_block_start` tool_use: keep the timing line OUT (the bridge sets `timing.tool`), and push a real block:
       ```ts
       } else if (b?.type === "tool_use") {
         const part = { type: "toolCall", id: b.id, name: String(b.name).replace(TOOL_PREFIX, ""), arguments: {} as any, json: "" };
         cur.msg.content.push(part);
         cur.parts.set(ev.index, part);
       }
       ```
     - `content_block_delta`: add `else if (d?.type === "signature_delta") { part.thinkingSignature += d.signature; break; }` before the `input_json_delta` branch.
     - `content_block_stop`:
       ```ts
       case "content_block_stop": {
         const part = cur?.parts.get(ev.index);
         if (part?.type === "toolCall") {
           try {
             part.arguments = part.json ? JSON.parse(part.json) : {};
           } catch {}
           delete part.json;
         }
         break;
       }
       ```
     - `message_delta`: add stop reason mapping:
       ```ts
       const sr = ev.delta?.stop_reason;
       if (sr) cur.msg.stopReason = sr === "tool_use" ? "toolUse" : sr === "max_tokens" ? "length" : "stop";
       ```
   - `onAssistant`: replace the `tool_use` branch with
     `else if (b.type === "tool_use") msg.content.push({ type: "toolCall", id: b.id, name: String(b.name).replace(TOOL_PREFIX, ""), arguments: b.input ?? {} });`
     and the thinking branch with `{ type: "thinking", thinking: b.thinking, thinkingSignature: b.signature ?? "" }`; replace its two `emit(message_end)` lines with: set `cur = { msg, parts: new Map(), input: 0 }` then `endCur()` (so it records and resolves waiters), keeping `message_update` before.
   - Delete `onUser` and the `case "user"` in `handle`.
   - `handle` `system`: delete the `init` branch (no session id is kept).
   - `start()`:
     ```ts
     function start() {
       const stream = new Pushable<SDKUserMessage>();
       const cwd = opts.cwd ?? process.cwd();
       // a fresh id per query; Claude Code reads pi's record for it once, before the child spawns
       const resumeId = randomUUID();
       const history = toClaudeEntries(piSession.sessionManager.buildSessionContext().messages, { sessionId: resumeId, cwd, model });
       const options: Options = {
         includePartialMessages: true,
         permissionMode: "bypassPermissions",
         allowDangerouslySkipPermissions: true,
         tools: [],
         // ponytail: one day per call so a long `bash` is never cut off by
         // Claude Code's MCP timeout; our abort is the real bound
         mcpServers: { kl: { type: "sdk", name: "kl", instance: toolServer() as any, timeout: 86_400_000 } as any },
         systemPrompt: piSession.systemPrompt,
         settingSources: [],
         cwd,
         model,
         extraArgs: { "thinking-display": "summarized" },
         env: claudeEnv(),
         sessionStore: {
           append: async () => {},
           load: async (k: any) => (k.sessionId === resumeId && !k.subpath ? history : null),
         } as any,
         ...(effort ? { effort } : {}),
         ...(history.length ? { resume: resumeId } : {}),
       };
       ...
     ```
     The rest of `start()` stays. In the loop's tail (child gone), add `turn.abort();` before `if (!disposed) finish();` so a running tool's `execute` stops.
     If `tsc` rejects `timeout` inside `McpSdkServerConfigWithInstance`, keep the `as any` shown; do not drop the field.
   - `send`: when `!running`, set `turn = new AbortController();` before `running = true`; record the user message before pushing:
     ```ts
     record({ role: "user", content: [...(images ?? []), { type: "text", text }], timestamp: stamp() });
     ```
   - `abort()`: add `turn.abort();` before `await q.interrupt()`.
   - Returned object: `agent: piSession.agent,` (delete the old `agent` line and its comment); `get messages() { return piSession.agent.state.messages; }`; in `dispose()` add `unsubPi(); turn.abort();` and `piSession.dispose?.();`.

6. Update `isClaude` doc nothing; keep `isClaude: true as const`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd harness && bun test packages/agent/src/claude.test.ts`
Expected: PASS, all (the old "thinking levels map to effort" test is untouched until Task 4 and still passes). All green before moving on.

Then: `cd harness && bunx tsc --noEmit -p packages/agent/tsconfig.json` (if the package has no tsconfig, run `bun run check` and read only the agent package's errors). Expected: no errors in `claude.ts`, `claude-tools.ts`, `claude-history.ts`. `index.ts` errors about `store`/`fresh` are expected until Task 5; note them, do not fix here.

- [ ] **Step 5: Commit**

```bash
cd /Volumes/kdisk/rustic-git-wt/master
git add harness/packages/agent/src/claude.ts harness/packages/agent/src/claude.test.ts
git commit -m "Run Claude sessions on pi's tools, prompt and transcript"
```

---

### Task 4: parity: steers, tokens, thinking, compaction, retries

**Files:**
- Modify: `harness/packages/agent/src/claude.ts`
- Test: `harness/packages/agent/src/claude.test.ts`

**Interfaces:**
- Consumes: Task 3's `record`, `send`, `finish`, bridge `onResult`.
- Produces: no new exports; `effortFor` changes behaviour; new `thinkingFor(level)` internal.

- [ ] **Step 1: Write the failing tests**

Replace the "thinking levels map to effort" test (line 178) with:

```ts
test("thinking levels map 1:1; only minimal falls back to low; off disables thinking", () => {
  expect(["off", "minimal", "low", "medium", "high", "xhigh", "max"].map((l) => effortFor(l as never))).toEqual([
    undefined, "low", "low", "medium", "high", "xhigh", "max",
  ]);
});

test("off sends thinking disabled", async () => {
  const f = fake((_n, push) => text("x").forEach(push));
  const { s } = run(f, { thinkingLevel: "off" });
  await s.prompt("a");
  expect(f.calls[0].options.thinking).toEqual({ type: "disabled" });
  expect(f.calls[0].options.effort).toBeUndefined();
  s.dispose();
});
```

Add:

```ts
test("token total counts cache reads and writes", async () => {
  const f = fake((_n, push) => {
    push({ type: "stream_event", event: { type: "message_start", message: { id: "c", usage: { input_tokens: 3, cache_read_input_tokens: 10, cache_creation_input_tokens: 5 } } } });
    push({ type: "stream_event", event: { type: "message_delta", usage: { output_tokens: 4 } } });
    push({ type: "stream_event", event: { type: "message_stop" } });
    push({ type: "result", subtype: "success" });
  });
  const { s, events } = run(f);
  await s.prompt("a");
  await tick();
  const end = events.find((e) => e.type === "message_end");
  expect(end.message.usage).toMatchObject({ input: 3, output: 4, cacheRead: 10, cacheWrite: 5, totalTokens: 22 });
  s.dispose();
});

test("a steer is held until a tool result or the turn end, and clearQueue drops it", async () => {
  let pushOut: (m: any) => void = () => {};
  const f = fake((_n, push) => {
    pushOut = push;
    toolTurn(["tu1"], push);
  });
  const { s, events, p } = run(f);
  p.tools.push({ name: "bash", description: "d", parameters: { type: "object" }, execute: async () => ({ content: [] }) });
  await s.prompt("go");
  await tick();
  const pushed = () => p.recorded.filter((m) => m.role === "user").map((m) => m.content.at(-1).text);
  await s.steer("drop me");
  expect(events.at(-1)).toMatchObject({ type: "queue_update", steering: ["drop me"] });
  s.clearQueue();
  expect(events.at(-1)).toMatchObject({ type: "queue_update", steering: [], followUp: [] });
  await s.steer("keep me");
  expect(pushed()).toEqual(["go"]);
  const c = await mcpOf(f.calls[0].options);
  await c.callTool({ name: "bash", arguments: {}, _meta: { "claudecode/toolUseId": "tu1" } });
  expect(pushed()).toEqual(["go", "keep me"]);
  expect(events.at(-1)).toMatchObject({ type: "queue_update", steering: [] });
  pushOut({ type: "result", subtype: "success" });
  s.dispose();
});

test("a steer still held at turn end becomes the next turn", async () => {
  const f = fake((n, push) => text(`r${n}`).forEach(push));
  const { s, events, p } = run(f);
  await s.prompt("a");
  await s.steer("b");
  await tick();
  await tick();
  expect(p.recorded.filter((m) => m.role === "user").map((m) => m.content.at(-1).text)).toEqual(["a", "b"]);
  expect(events.filter((e) => e.type === "agent_end").length).toBe(2);
  s.dispose();
});

test("auto-compaction toggles live and compaction and retries become pi events", async () => {
  const applied: any[] = [];
  const f = fake((_n, push) => {
    push({ type: "system", subtype: "status", status: "compacting" });
    push({ type: "system", subtype: "compact_boundary", compact_metadata: { trigger: "auto", pre_tokens: 9000, post_tokens: 1200 } });
    push({ type: "system", subtype: "api_retry", attempt: 1, max_retries: 3, retry_delay_ms: 500, error_status: 529, error: "overloaded" });
    text("ok").forEach(push);
  });
  const { s, events } = run(f);
  await s.prompt("a");
  await tick();
  expect(events.find((e) => e.type === "compaction_start")).toMatchObject({ reason: "threshold" });
  expect(events.find((e) => e.type === "compaction_end")).toMatchObject({
    reason: "threshold", aborted: false, willRetry: false, result: { tokensBefore: 9000, estimatedTokensAfter: 1200 },
  });
  expect(events.find((e) => e.type === "auto_retry_start")).toMatchObject({ attempt: 1, maxAttempts: 3, delayMs: 500, errorMessage: "overloaded" });
  expect(events.find((e) => e.type === "auto_retry_end")).toMatchObject({ success: true, attempt: 1 });
  s.setAutoCompactionEnabled(false);
  await tick();
  expect(f.flags.at(-1)).toEqual({ autoCompactEnabled: false });
  s.dispose();
});
```

For that last test, extend `fake()` so `applyFlagSettings` records: add `const flags: any[] = [];` beside `calls`, make it `applyFlagSettings: async (x: any) => { flags.push(x); }`, and return `{ query, calls, interrupts, flags }`.

Also in the "two prompts are two turns" test, line 78 stays `expect(ends[0].message.usage.totalTokens).toBe(7);`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd harness && bun test packages/agent/src/claude.test.ts`
Expected: FAIL in the thinking, token, steer, compaction tests.

- [ ] **Step 3: Implement**

In `claude.ts`:

1. `effortFor` (lines 45-51):
   ```ts
   /** 1:1 onto the SDK's effort; `minimal` has no match and takes `low`; `off` is no effort (thinking disabled). */
   export function effortFor(level: Level | undefined): Effort | undefined {
     if (!level || level === "off") return undefined;
     return level === "minimal" ? "low" : level;
   }
   ```
   Add a session-level `let thinkingOff = opts.thinkingLevel === "off";` next to `effort`. In `start()` options add `...(thinkingOff ? { thinking: { type: "disabled" } } : {})`. In `setThinkingLevel`: `thinkingOff = level === "off"; effort = effortFor(level);` then keep the `applyFlagSettings({ effortLevel: effort ?? null })` call. (A live switch to/from `off` takes effect on the next query: `ponytail:` comment saying the SDK has no live thinking toggle, the next child picks it up.)

2. Tokens: add a helper inside the session and use it at `message_start` and `message_delta`:
   ```ts
   // pi's formula (pi-ai anthropic-messages): every token the request carried
   const usage = (u: any) => {
     const x = cur!.msg.usage;
     if (u.input_tokens != null) x.input = u.input_tokens;
     if (u.output_tokens != null) x.output = u.output_tokens;
     if (u.cache_read_input_tokens != null) x.cacheRead = u.cache_read_input_tokens;
     if (u.cache_creation_input_tokens != null) x.cacheWrite = u.cache_creation_input_tokens;
     x.totalTokens = x.input + x.output + x.cacheRead + x.cacheWrite;
   };
   ```
   `message_start`: `if (ev.message?.usage) usage(ev.message.usage);` replacing the old `totalTokens` line. `message_delta`: `if (u) usage(u);` replacing its two lines. Remove `input` from the `Cur` type and its uses.

3. Steers held:
   - Change `const steering: string[] = [];` to `const steering: { text: string; images?: Image[] }[] = [];` and `queueUpdate` to map `steering.map((s) => s.text)`.
   - New:
     ```ts
     /** Held steers go in at a boundary we choose, so clearQueue can still recall them. */
     function flushSteers() {
       if (!steering.length || !input) return;
       for (const s of steering.splice(0)) pushUser(s.text, s.images);
       queueUpdate();
     }
     ```
   - Split `send`: extract the content-build + `record(...)` + `input!.push(...)` into `function pushUser(text: string, images?: Image[])`; `send` becomes `if (!q) start(); if (!running) {...turn start...} pushUser(text, images);`.
   - `steer`:
     ```ts
     async steer(text: string, images?: Image[]) {
       if (!running) return send(text, images);
       steering.push({ text, images });
       queueUpdate();
     },
     ```
   - In the bridge host: `onResult: (m) => { record(m); flushSteers(); }`.
   - Delete the "the turn has moved on" block in `message_start`.
   - `finish`: before `const next = followUps.shift();` add:
     ```ts
     const held = steering.splice(0);
     if (held.length) {
       queueUpdate();
       for (const s of held) void send(s.text, s.images);
       return;
     }
     ```
   - `clearQueue`: `steering.length = 0; followUps.length = 0; queueUpdate();`.
   - `abort`: also `steering.length = 0;`.

4. Auto-compaction: replace the no-op with
   ```ts
   /** Live: Claude Code compacts, we only switch it. */
   setAutoCompactionEnabled(on: boolean) {
     autoCompact = on;
     void q?.applyFlagSettings({ autoCompactEnabled: on }).catch(() => {});
   },
   ```
   with `let autoCompact: boolean | undefined;` in the session, and in `start()` after `q` is created: `if (autoCompact !== undefined) void q.applyFlagSettings({ autoCompactEnabled: autoCompact }).catch(() => {});`.

5. Compaction and retry events, in `handle` `case "system"`:
   ```ts
   } else if (m.subtype === "status" && m.status === "compacting") {
     compacting = true;
     emit({ type: "compaction_start", reason: "threshold" });
   } else if (m.subtype === "compact_boundary") {
     compacting = false;
     const meta = m.compact_metadata ?? {};
     emit({
       type: "compaction_end",
       reason: meta.trigger === "manual" ? "manual" : "threshold",
       result: { tokensBefore: meta.pre_tokens, estimatedTokensAfter: meta.post_tokens },
       aborted: false,
       willRetry: false,
     });
   } else if (m.subtype === "api_retry") {
     retry = m.attempt;
     emit({ type: "auto_retry_start", attempt: m.attempt, maxAttempts: m.max_retries, delayMs: m.retry_delay_ms, errorMessage: String(m.error ?? m.error_status ?? "") });
   }
   ```
   with `let compacting = false; let retry: number | undefined;` in the session. Close a retry: in `onStream` `message_start`, `if (retry !== undefined) { emit({ type: "auto_retry_end", success: true, attempt: retry }); retry = undefined; }`; in `onResult` error branch, `if (retry !== undefined) { emit({ type: "auto_retry_end", success: false, attempt: retry, finalError: text }); retry = undefined; }` (after `text` is computed, only when `!aborting`). In `finish()`, if `compacting` is still true emit `compaction_end` with `aborted: true` and reset it.

   ponytail comment above the block: "when to compact, what the summary says and the retry policy are Claude Code's; we only report them."

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd harness && bun test packages/agent/src/claude.test.ts packages/agent/src/claude-tools.test.ts packages/agent/src/claude-history.test.ts`
Expected: PASS, all.

- [ ] **Step 5: Commit**

```bash
cd /Volumes/kdisk/rustic-git-wt/master
git add harness/packages/agent/src/claude.ts harness/packages/agent/src/claude.test.ts
git commit -m "Match pi on steers, tokens, thinking, compaction and retries for Claude"
```

---

### Task 5: one pi session for every model

**Files:**
- Modify: `harness/packages/agent/src/index.ts:199` (`SessionMeta`), `:274-349` (`createSession`)
- Modify: `harness/apps/tui/src/app.tsx:1189-1190` (comment)
- Test: `harness/packages/agent/src/index.test.ts` (create if absent; if a test file for `index.ts` exists, add to it)

**Interfaces:**
- Consumes: `createClaudeSession({ key, model, thinkingLevel, pi })` (Task 3), `declaredTools` (Task 2).
- Produces: `createSession` signature unchanged; for anthropic it returns a `ClaudeSession` whose `pi` is a full pi `AgentSession` built with the same registry, codemode and `SessionManager`.

- [ ] **Step 1: Write the failing test**

`harness/packages/agent/src/index.test.ts` (check first: `ls harness/packages/agent/src/index.test.ts`; append if it exists):

```ts
import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { declaredTools } from "./claude-tools.ts";

test("codemode only: pi declares codemode, not bash (the Claude bridge relies on it)", async () => {
  process.env.XDG_CONFIG_HOME = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  const { createSession } = await import("./index.ts");
  const { getModel } = await import("@earendil-works/pi-ai");
  const model = getModel("openai", "gpt-5" as never) as any;
  const s: any = await createSession({ key: `t-${process.pid}`, model, codemode: true, fresh: true });
  const names = declaredTools(s).map((t: any) => t.name);
  expect(names).toContain("codemode");
  expect(names).not.toContain("bash");
  expect(s.agent.state.tools.map((t: any) => t.name)).toContain("bash"); // still callable from scripts
  expect(s.systemPrompt).toContain("codemode");
  s.dispose();
});
```

If `index.ts`'s `CONFIG_DIR` is not derived from `XDG_CONFIG_HOME`, read how it is computed (top of `index.ts`) and point it at a temp dir the same way it reads its env; never write under the real `~/.config/kloudlite`. If `getModel("openai","gpt-5")` is not in pi-ai's catalogue, use any non-anthropic model id the catalogue has (`bun -e 'import {getModels} from "@earendil-works/pi-ai"; console.log(getModels("openai").map(m=>m.id).slice(0,5))'`).

- [ ] **Step 2: Run test to verify it passes or fails for the right reason**

Run: `cd harness && bun test packages/agent/src/index.test.ts`
Expected: PASS already for the pi path (this test pins the private-field contract the bridge depends on). If it fails on `_hiddenDeclarations`, STOP: the harvest rule is wrong for this pi build; report with the failing line.

- [ ] **Step 3: Rewire `createSession`**

In `index.ts`:

1. `SessionMeta` (line 199): remove `claudeSessionId?: string`.
2. Delete the `if (model.provider === "anthropic") { ... }` block (lines 297-310).
3. After `if (autoCompact !== undefined) session.setAutoCompactionEnabled(autoCompact);` replace `return session;` with:
   ```ts
   // Claude models run Claude Code's loop on this same session: its tools,
   // prompt and transcript (spec 2026-10-08-claude-tool-host). pi's own loop
   // never starts for them.
   if (model.provider === "anthropic") {
     const claude = createClaudeSession({ key, model, thinkingLevel, pi: session as never });
     if (autoCompact !== undefined) claude.setAutoCompactionEnabled(autoCompact);
     return claude;
   }
   return session;
   ```
4. Update the comment at the old `writeMeta` line (311) to cover both families; the anthropic `writeMeta` duplicate is gone with the deleted block.
5. `createAgentSession` is given `model` for anthropic too. pi must not need anthropic credentials to BUILD a session (it only needs them to stream). If `createAgentSession` throws for an anthropic model without auth, pass the model anyway and catch nothing: report it as BLOCKED with the error text, do not add a fallback model.

In `apps/tui/src/app.tsx` lines 1189-1190 replace the comment with:
```ts
          // Claude and pi share one transcript (pi's file): a switch across
          // the two rebuilds the session for this key from that file
```

- [ ] **Step 4: Run the gate**

Run: `cd harness && bun test packages/agent && bun run check`
Expected: agent tests PASS; `check` green except the one known `Transcript` test. Any TS error mentioning `store`, `fresh` or `claudeSessionId` means a caller still passes them: fix the caller (search `grep -rn "claudeSessionId\|store:" harness/packages harness/apps --include=*.ts --include=*.tsx`).

- [ ] **Step 5: Commit**

```bash
cd /Volumes/kdisk/rustic-git-wt/master
git add harness/packages/agent/src/index.ts harness/packages/agent/src/index.test.ts harness/apps/tui/src/app.tsx
git commit -m "Build pi's session for every model and run Claude on it"
```

---

### Task 6: live smoke on the laptop

**Files:**
- Create (scratch, deleted after): `$SCRATCH/claude-smoke.ts` where `SCRATCH=/private/tmp/claude-501/-Users-karthik-rustic-git/ca5119e6-5c89-409b-a058-b668d4457515/scratchpad`

**Interfaces:**
- Consumes: `createSession` from `harness/packages/agent/src/index.ts`.

- [ ] **Step 1: Write the smoke script**

```ts
// run from harness/: bun $SCRATCH/claude-smoke.ts
import { createSession } from "/Volumes/kdisk/rustic-git-wt/master/harness/packages/agent/src/index.ts";
import { getModel } from "@earendil-works/pi-ai";

const key = `smoke-${Date.now()}`;
const wait = (s: any) => new Promise<void>((r) => { const off = s.subscribe((e: any) => e.type === "agent_end" && (off(), r())); });
const say = async (s: any, t: string) => { const done = wait(s); await s.prompt(t); await done; const a = s.messages.findLast((m: any) => m.role === "assistant"); return a.content.filter((b: any) => b.type === "text").map((b: any) => b.text).join(""); };

const piModel = getModel(process.env.SMOKE_PI_PROVIDER ?? "openai", (process.env.SMOKE_PI_MODEL ?? "gpt-5-mini") as never) as any;
const claudeModel = { ...getModel("anthropic", "claude-haiku-4-5" as never) } as any;

let s: any = await createSession({ key, model: piModel, codemode: true, fresh: true });
console.log("pi 1:", await say(s, "The secret word is PELICAN. Run `ls /` with a tool and tell me one entry."));
s.dispose();

s = await createSession({ key, model: claudeModel, codemode: true });
console.log("claude 1:", await say(s, "What was the secret word, and which entry of / did you report?"));
console.log("claude 2:", await say(s, "Run `echo hi` with a tool and give me its output."));
const t0 = Date.now();
const done = wait(s);
await s.prompt("Run `sleep 30` with a tool.");
setTimeout(() => s.abort(), 5_000);
await done;
console.log("abort took ms:", Date.now() - t0);
s.dispose();

s = await createSession({ key, model: piModel, codemode: true });
console.log("pi 2:", await say(s, "What did `echo hi` print earlier?"));
s.dispose();
```

- [ ] **Step 2: Run it**

Run: `cd /Volumes/kdisk/rustic-git-wt/master/harness && bun $SCRATCH/claude-smoke.ts 2>&1 | tail -20`
Then: `pgrep -fl "claude-agent-sdk.*claude" | wc -l` during the Claude turns must show one child (check by running the script with a `await new Promise(r=>setTimeout(r,3000))` between `claude 1` and `claude 2` and running `pgrep` in another shell; or count `system/init` messages: add `s.subscribe` is not enough, so use `pgrep`).
Expected:
- `claude 1` names PELICAN and a real `/` entry (history crossed pi to Claude).
- `claude 2` prints `hi` (a pi tool ran through the bridge; codemode or bash).
- abort under ~7000 ms.
- `pi 2` says `hi` (history crossed Claude to pi).

- [ ] **Step 3: Permission card check (manual, in the TUI)**

Run the TUI on the laptop (`cd harness && bun run dev` or the app's start script), pick `claude-haiku-4-5`, switch to plan mode, ask it to run `ls`: the model must receive the plan-mode refusal (it explains instead of running). Switch to default mode, ask again: a "Permission required / Shell command" card appears. Note: under codemode `only`, `bash` runs nested inside codemode and the TUI gate does not cover nested calls under pi either (parity); test with codemode off (`/codemode off` or prefs) for the card.

- [ ] **Step 4: Clean up**

```bash
rm $SCRATCH/claude-smoke.ts
rm -rf ~/.config/kloudlite/sessions/smoke-*
```

Record the four results (PELICAN, hi, abort ms, pi 2) in the ledger `HANDOFF-merge-master.md`. Nothing to commit.

---

## Self-review notes

- Spec §1 build pi first: Task 5. §2 bridge: Task 2 + wiring Task 3. §3 parity: Task 4 (steers, tokens, thinking, autoCompact, events). §4 transcript: Task 1 (to entries), Task 3 (record, `sessionStore`, drop `store`). §5 abort/child death: Tasks 2-3 tests. Tests section: Tasks 1-4, live smoke Task 6.
- Spec says `createSdkMcpServer`; ruled to low-level `Server` (Rulings).
- Spec's history test "and back" ruled to forward-only + smoke (Rulings).
