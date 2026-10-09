import { expect, test } from "bun:test";
import { connect, RemoteBackend } from "./remote.ts";
import { serve } from "./serve.ts";
import { Peer } from "./wire.ts";
import type { Backend, SessionOpts } from "./index.ts";

const enc = new TextEncoder();

/** A Backend whose session drives the callbacks the way pi would. */
function fake(): Backend & { seen: string[] } {
  const seen: string[] = [];
  const subs = new Set<(e: any) => void>();
  let opts!: SessionOpts;
  const b: any = {
    seen,
    hello: async () => ({ protocol: 2, cwd: "/home/kl", tools: [] }),
    session: async (_key: string, o: SessionOpts) => {
      opts = o;
      return {
        messages: [{ role: "user", content: "earlier" }] as any,
        isClaude: false,
        prompt: async (text: string) => {
          seen.push(`tool:${await opts.tools[0]!.run({ q: text })}`);
          for (const s of subs) s({ type: "agent_end", error: new Error("e") });
        },
        abort: async () => void seen.push("abort"),
        dispose: async () => void seen.push("dispose"),
        subscribe: (cb: any) => (subs.add(cb), () => subs.delete(cb)),
      };
    },
    sessions: {}, fs: {}, podfs: {}, settings: {}, models: {}, auth: {}, space: async () => ({ available: false, error: "x", user: "u", workspaces: [], environments: [] }),
  };
  return b;
}

function wired(backend: Backend) {
  const client: Peer = new Peer((l) => queueMicrotask(() => host.feed(enc.encode(l))));
  const host: Peer = new Peer((l) => queueMicrotask(() => client.feed(enc.encode(l))));
  serve(backend, host);
  return { remote: new RemoteBackend(client), client, host };
}

test("session round trip: messages, TUI tool, events with Error", async () => {
  const b = fake();
  const { remote } = wired(b);
  const events: any[] = [];
  const h = await remote.session("k", {
    initial: { model: { provider: "p", id: "m" } },
    tools: [{ name: "question", description: "", inputSchema: { type: "object" }, run: async (i: any) => `answered ${i.q}` }],
  });
  expect(h.messages).toEqual([{ role: "user", content: "earlier" }] as any);
  h.subscribe((e) => events.push(e));
  await h.prompt("ls");
  expect(b.seen).toEqual(["tool:answered ls"]);
  await Bun.sleep(5);
  expect(events[0].error).toEqual({ name: "Error", message: "e" });
});

test("disconnect rejects pending calls", async () => {
  const b = fake();
  const { remote, client } = wired(b);
  const h = await remote.session("k", { initial: { model: { provider: "p", id: "m" } }, tools: [{ name: "question", description: "", inputSchema: { type: "object" }, run: () => new Promise<string>(() => {}) }] });
  const p = h.prompt("ls");
  await Bun.sleep(5);
  client.close();
  await expect(p).rejects.toThrow("disconnected");
});

test("connect() rejects with code 3 when the command exits before hello", async () => {
  const e: any = await connect(["sh", "-c", "echo 'kl bench: unknown command' >&2; exit 2"]).catch((x) => x);
  expect(e.code).toBe(3);
});

test("connect() rejects with code 3 on a protocol mismatch", async () => {
  const e: any = await connect([
    "sh", "-c", `read l; echo '{"re":1,"ok":true,"value":{"protocol":99}}'; sleep 1`,
  ]).catch((x) => x);
  expect(e.code).toBe(3);
});

// Two pipes carry no order: "later" must follow hello by a beat or it can reach the stderr reader before `booted` flips.
test("stderr passes through before hello and is buffered after", async () => {
  const c = await connect([
    "sh", "-c",
    `echo waking >&2; read l; echo '{"re":1,"ok":true,"value":{"protocol":2}}'; sleep 0.1; echo later >&2; sleep 0.2`,
  ]);
  await c.exited;
  expect(c.stderr()).toContain("later");
  expect(c.stderr()).not.toContain("waking");
});

test("btw returns its string over the wire", async () => {
  const b = fake();
  const base = b.session;
  b.session = async (k: string, o: SessionOpts) => ({ ...(await base(k, o)), btw: async (q: string) => `answer to ${q}` }) as any;
  const { remote } = wired(b);
  const h = await remote.session("k", { initial: { model: { provider: "p", id: "m" } }, tools: [] });
  expect(await h.btw("why?")).toBe("answer to why?");
});
