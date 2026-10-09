import { test, expect } from "bun:test";
import { Cards } from "./cards";

const base = { key: "main", kind: "permission" as const, tool: "bash", title: "Permission required", options: [{ id: "once", label: "Allow once" }, { id: "reject", label: "Reject" }] };

test("first answer wins, every connection hears ask and ask_resolved, a late answer is ignored", async () => {
  const seen: any[] = [];
  const c = new Cards((e) => seen.push(e));
  const p = c.ask(base, new AbortController().signal, "reject");
  const id = seen[0].ask.id;
  expect(seen[0].type).toBe("ask");
  expect(c.pending().map((a) => a.id)).toEqual([id]);
  c.answer(id, "once");
  c.answer(id, "reject");
  expect(await p).toBe("once");
  expect(seen.filter((e) => e.type === "ask_resolved")).toEqual([{ type: "ask_resolved", id }]);
  expect(c.pending()).toEqual([]);
});

test("abort withdraws with the fallback and resolves the card", async () => {
  const seen: any[] = [];
  const c = new Cards((e) => seen.push(e));
  const ac = new AbortController();
  const p = c.ask(base, ac.signal, "reject");
  ac.abort();
  expect(await p).toBe("reject");
  expect(seen.at(-1)).toEqual({ type: "ask_resolved", id: seen[0].ask.id });
});

test("withdrawKey resolves every ask of that key only", async () => {
  const c = new Cards(() => {});
  const a = c.ask(base, new AbortController().signal, "reject");
  const b = c.ask({ ...base, key: "ws-1" }, new AbortController().signal, "reject");
  c.withdrawKey("main");
  expect(await a).toBe("reject");
  expect(c.pending().map((x) => x.key)).toEqual(["ws-1"]);
  c.withdrawKey("ws-1");
  await b;
});
