import { expect, test } from "bun:test";
import { join } from "node:path";
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
    hello: async () => ({ protocol: 1, cwd: "/home/kl", tools: [] }),
    session: async (_key: string, o: SessionOpts) => {
      opts = o;
      return {
        messages: [{ role: "user", content: "earlier" }] as any,
        isClaude: false,
        prompt: async (text: string) => {
          const d = await opts.permission({ name: "bash", args: { command: text } }, new AbortController().signal);
          seen.push(`decision:${d.block ? "block" : "allow"}`);
          seen.push(`tool:${await opts.tools[0]!.run({ q: text })}`);
          for (const s of subs) s({ type: "agent_end", error: new Error("e") });
        },
        abort: async () => void seen.push("abort"),
        dispose: async () => void seen.push("dispose"),
        subscribe: (cb: any) => (subs.add(cb), () => subs.delete(cb)),
      };
    },
    // a permission call that never answers until aborted
    _hang: (signal: AbortSignal) => opts.permission({ name: "bash", args: {} }, signal),
    sessions: {}, fs: {}, settings: {}, models: {}, auth: {},
  };
  return b;
}

function wired(backend: Backend) {
  const client: Peer = new Peer((l) => queueMicrotask(() => host.feed(enc.encode(l))));
  const host: Peer = new Peer((l) => queueMicrotask(() => client.feed(enc.encode(l))));
  serve(backend, host);
  return { remote: new RemoteBackend(client), client, host };
}

test("session round trip: messages, permission, TUI tool, events with Error", async () => {
  const b = fake();
  const { remote } = wired(b);
  const events: any[] = [];
  const h = await remote.session("k", {
    model: { provider: "p", id: "m" },
    tools: [{ name: "question", description: "", inputSchema: {}, run: async (i: any) => `answered ${i.q}` }],
    permission: async (req) => ({ block: req.args.command === "rm" }),
  });
  expect(h.messages).toEqual([{ role: "user", content: "earlier" }] as any);
  h.subscribe((e) => events.push(e));
  await h.prompt("ls");
  expect(b.seen).toEqual(["decision:allow", "tool:answered ls"]);
  await Bun.sleep(5);
  expect(events[0].error).toEqual({ name: "Error", message: "e" });
});

test("abort during permission sends cancel; late answer ignored", async () => {
  const b: any = fake();
  const { remote } = wired(b);
  let answer!: (d: any) => void;
  let clientSignal!: AbortSignal;
  await remote.session("k", {
    model: { provider: "p", id: "m" },
    tools: [],
    permission: (_req, signal) => ((clientSignal = signal), new Promise((r) => (answer = r))),
  });
  const ac = new AbortController();
  const pending = b._hang(ac.signal);
  await Bun.sleep(5);
  ac.abort();
  await expect(pending).rejects.toThrow();
  await Bun.sleep(5);
  expect(clientSignal.aborted).toBe(true);
  answer({}); // the card's late answer: dropped, no throw
  await Bun.sleep(5);
});

test("disconnect rejects pending calls", async () => {
  const b = fake();
  const { remote, client } = wired(b);
  const h = await remote.session("k", { model: { provider: "p", id: "m" }, tools: [], permission: () => new Promise(() => {}) });
  const p = h.prompt("ls");
  await Bun.sleep(5);
  client.close();
  await expect(p).rejects.toThrow("disconnected");
});

test("connect() over a real child: hello from serve.ts", async () => {
  const c = await connect(["bun", "run", "--silent", join(import.meta.dir, "serve.ts")]);
  expect(c.hello.protocol).toBe(1);
  expect(c.hello.cwd).toBe(process.cwd());
  expect(await c.backend.fs.isGitRepo(process.cwd())).toBeTypeOf("boolean");
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

test("stderr passes through before hello and is buffered after", async () => {
  const c = await connect([
    "sh", "-c",
    `echo waking >&2; read l; echo '{"re":1,"ok":true,"value":{"protocol":1}}'; echo later >&2; sleep 0.2`,
  ]);
  await c.exited;
  expect(c.stderr()).toContain("later");
  expect(c.stderr()).not.toContain("waking");
});
