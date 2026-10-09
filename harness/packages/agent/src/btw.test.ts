import { expect, test } from "bun:test";
import { btwPrompt, piBtw } from "./btw.ts";

const user = (text: string) => ({ role: "user", content: [{ type: "text", text }], timestamp: 1 });
const assistant = (text: string) => ({
  role: "assistant", content: [{ type: "text", text }], api: "x", provider: "p", model: "m",
  usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
  stopReason: "stop", timestamp: 2,
});

test("btwPrompt carries the conversation, the question and the instruction", () => {
  const p = btwPrompt([user("fix the cache"), assistant("done")], "why a map?", 100_000);
  expect(p).toContain("fix the cache");
  expect(p).toContain("<question>\nwhy a map?\n</question>");
  expect(p).toContain("You have no tools. Do not offer to do work.");
  expect(p).not.toContain("Note: only");
});

test("btwPrompt keeps the newest messages within the budget and says so", () => {
  const msgs = Array.from({ length: 20 }, (_, i) => user(`message-${i} ${"x".repeat(400)}`));
  const p = btwPrompt(msgs, "q", 1000); // budget 600 tokens
  expect(p).toMatch(/Note: only the last \d+ of 20 messages are shown\./);
  expect(p).toContain("message-19");
  expect(p).not.toContain("message-0 ");
});

function fakePi(result: any) {
  const messages = [user("hello"), assistant("hi")];
  const seen: any[] = [];
  const session = {
    model: { id: "m" },
    messages,
    _getSummarizationRequestAuth: async () => ({ model: { id: "m", contextWindow: 100_000, maxTokens: 8000 }, apiKey: "k" }),
    agent: { streamFunction: async (...a: any[]) => (seen.push(a), { result: async () => result }) },
  };
  return { session, messages, seen };
}

test("piBtw returns the text, sends one tool-less user message and leaves the session alone", async () => {
  const f = fakePi({ stopReason: "stop", content: [{ type: "text", text: " Because. " }] });
  const before = [...f.messages];
  expect(await piBtw(f.session, "why?")).toBe("Because.");
  expect(f.messages).toEqual(before);
  expect(f.messages.length).toBe(2);
  const [, ctx, opts] = f.seen[0];
  expect(ctx.messages).toHaveLength(1);
  expect(ctx.tools).toBeUndefined();
  expect(opts).toMatchObject({ apiKey: "k", maxTokens: 4096, cacheRetention: "none" });
});

test("piBtw: an error stop reason throws; empty text is a placeholder; no model throws", async () => {
  await expect(piBtw(fakePi({ stopReason: "error", errorMessage: "boom", content: [] }).session, "q")).rejects.toThrow("boom");
  expect(await piBtw(fakePi({ stopReason: "stop", content: [] }).session, "q")).toBe("(no answer)");
  const f = fakePi({});
  (f.session as any).model = undefined;
  await expect(piBtw(f.session, "q")).rejects.toThrow("No model selected");
});
