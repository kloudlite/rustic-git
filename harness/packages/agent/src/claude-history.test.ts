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
  // thinking from a non-Anthropic api is dropped
  expect(a.content).toEqual([
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

test("anthropic thinking keeps its signature; a GPT reasoning-item signature is dropped", () => {
  const mk = (api: string, sig: string) => ({
    role: "assistant",
    content: [{ type: "thinking", thinking: "hm", thinkingSignature: sig }, { type: "text", text: "ok" }],
    api, provider: "x", model: "m", usage, stopReason: "stop", timestamp: 1,
  });
  const gpt = toClaudeEntries([mk("openai-responses", '{"type":"reasoning","id":"rs_1"}')], o);
  expect((gpt[0].message as any).content).toEqual([{ type: "text", text: "ok" }]);
  const ant = toClaudeEntries([mk("anthropic-messages", "sig")], o);
  expect((ant[0].message as any).content[0]).toEqual({ type: "thinking", thinking: "hm", signature: "sig" });
  expect((ant[0].message as any).model).toBe("claude-haiku-4-5");
});

test("a transcript ending on a tool call gets a closing error result", () => {
  const e = toClaudeEntries(
    [
      { role: "user", content: "go", timestamp: 1 },
      { role: "assistant", content: [{ type: "toolCall", id: "tu7", name: "bash", arguments: {} }], api: "x", provider: "anthropic", model: "m", usage, stopReason: "toolUse", timestamp: 2 },
    ],
    o,
  );
  const last = e[e.length - 1];
  expect((last.message as any).content[0]).toMatchObject({ type: "tool_result", tool_use_id: "tu7", is_error: true });
  expect(last.sourceToolAssistantUUID).toBe(e[1].uuid);
});
