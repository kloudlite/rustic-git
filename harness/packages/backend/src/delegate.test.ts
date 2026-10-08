import { expect, test } from "bun:test";
import { delegateTools } from "./delegate.ts";
import type { SessionHandle, SessionOpts } from "./index.ts";

const caller = { model: { provider: "p", id: "m" }, tools: [], permission: async () => ({}) } as unknown as SessionOpts;

function fake(reply = "done") {
  const subs = new Set<(e: any) => void>();
  const sent: string[] = [];
  const fire = () => {
    for (const s of subs) {
      s({ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: reply }] } });
      s({ type: "agent_end", messages: [] });
    }
  };
  const h = {
    sent,
    disposed: 0,
    prompt: async (t: string) => void (sent.push(`prompt:${t}`), queueMicrotask(fire)),
    followUp: async (t: string) => void (sent.push(`followUp:${t}`), queueMicrotask(fire)),
    dispose: async () => void h.disposed++,
    subscribe: (cb: any) => (subs.add(cb), () => subs.delete(cb)),
  };
  return h as unknown as SessionHandle & { sent: string[]; disposed: number };
}

test("subagent runs in a fresh :agent- session and always disposes it", async () => {
  const h = fake("found it");
  const opened: { key: string; opts: SessionOpts }[] = [];
  const [, sub] = delegateTools("main", undefined, { live: new Map(), busy: new Set(), open: async (key, opts) => (opened.push({ key, opts }), h) }, caller);
  expect(await sub!.run({ workspace: "w1", task: "look" })).toBe("found it");
  expect(opened[0]!.key).toMatch(/^w1:agent-[0-9a-f]{8}$/);
  expect(opened[0]!.opts.fresh).toBe(true);
  expect(opened[0]!.opts.tools).toEqual([]);
  expect(h.disposed).toBe(1);
  const failing: any = fake();
  failing.prompt = async () => { throw new Error("boom"); };
  const [, sub2] = delegateTools("main", undefined, { live: new Map(), busy: new Set(), open: async () => failing }, caller);
  await expect(sub2!.run({ workspace: "w1", task: "x" })).rejects.toThrow("boom");
  expect(failing.disposed).toBe(1);
});

test("workspace_ask follows up a busy session, prompts an idle one, disposes only what it opened", async () => {
  const live = new Map<string, SessionHandle>();
  const busy = new Set<string>();
  const h = fake("ok");
  live.set("w1", h);
  const [ask] = delegateTools("main", undefined, { live, busy, open: async () => fake() }, caller);
  await ask!.run({ workspace: "w1", request: "a" });
  busy.add("w1");
  await ask!.run({ workspace: "w1", request: "b" });
  expect(h.sent).toEqual(["prompt:[from main session] a", "followUp:[from main session] b"]);
  expect(h.disposed).toBe(0);
  const opened = fake();
  const [ask2] = delegateTools("main", undefined, { live: new Map(), busy: new Set(), open: async () => opened }, caller);
  await ask2!.run({ workspace: "w2", request: "c" });
  expect(opened.disposed).toBe(1);
});
