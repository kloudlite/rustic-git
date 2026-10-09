import { expect, test } from "bun:test";
import { Clients, type Client } from "./clients.ts";

const mk = (name: string, f?: () => Promise<string>): Client => ({
  permission: async () => ({ reason: name }),
  tools: new Map([["question", f ?? (async () => name)]]),
});
const perm = (c: Client) => !!c.permission;
const ask = (c: Client) => c.permission!({ name: "bash", args: {} }, new AbortController().signal).then((d) => d.reason);

test("newest client wins", async () => {
  const cs = new Clients();
  cs.add("k", mk("old"));
  cs.add("k", mk("new"));
  expect(await cs.route("k", perm, ask)).toBe("new");
});

test("a disconnected client falls through to an older one", async () => {
  const cs = new Clients();
  cs.add("k", mk("old"));
  const dead: Client = { permission: async () => Promise.reject(new Error("disconnected")), tools: new Map() };
  cs.add("k", dead);
  expect(await cs.route("k", perm, ask)).toBe("old");
});

test("no client waits until one is added", async () => {
  const cs = new Clients();
  const p = cs.route("k", perm, ask);
  let done = false;
  void p.then(() => (done = true));
  await Bun.sleep(10);
  expect(done).toBe(false);
  cs.add("other", mk("elsewhere"));
  cs.add("k", mk("late"));
  expect(await p).toBe("late");
});

test("an aborted signal while waiting rejects", async () => {
  const cs = new Clients();
  const ac = new AbortController();
  const p = cs.route("k", perm, ask, ac.signal);
  ac.abort();
  await expect(p).rejects.toThrow("aborted");
});

test("any other error propagates", async () => {
  const cs = new Clients();
  cs.add("k", mk("old"));
  cs.add("k", { permission: async () => Promise.reject(new Error("boom")), tools: new Map() });
  await expect(cs.route("k", perm, ask)).rejects.toThrow("boom");
});

test("has filters: a client without the tool is skipped; unregister removes", async () => {
  const cs = new Clients();
  cs.add("k", mk("has"));
  const off = cs.add("k", { tools: new Map() });
  expect(await cs.route("k", (c) => c.tools.has("question"), (c) => c.tools.get("question")!({}))).toBe("has");
  off();
  const gone = cs.add("k", mk("newer"));
  gone();
  expect(await cs.route("k", perm, ask)).toBe("has");
});
