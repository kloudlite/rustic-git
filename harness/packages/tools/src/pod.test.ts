import { afterEach, beforeEach, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { podTools } from "./pod.ts";

const realFetch = globalThis.fetch;
let log: { url: string; auth: string | null }[] = [];
let handler: (url: string, init: any) => Response | Promise<Response>;

beforeEach(() => {
  log = [];
  const f = join(mkdtempSync(join(tmpdir(), "kl-tok-")), "t");
  writeFileSync(f, "bench");
  process.env.KL_API_URL = "http://api.test";
  process.env.KL_TOOL_TOKEN_FILE = f;
  globalThis.fetch = (async (url: string, init: any = {}) => {
    log.push({ url: String(url), auth: init.headers?.authorization ?? null });
    return handler(String(url), init);
  }) as any;
});
afterEach(() => {
  globalThis.fetch = realFetch;
});

const j = (v: unknown, status = 200) => new Response(JSON.stringify(v), { status });

test("schema comes from the pod, only for fixed names, token is sent", async () => {
  handler = (u) =>
    u.endsWith("/v1/workspaces/w1/tools") ? j({ address: "10.0.0.5:7788", token: "pt" })
    : u.endsWith("/tools") ? j({ tools: [{ name: "read", description: "Read a file", schema: { type: "object", properties: { path: {} } } }, { name: "bogus", schema: {} }] })
    : j({ ok: 1 });
  const t = await podTools("w1");
  expect(t.map((x) => x.name)).toEqual(["read"]);
  expect(t[0]!.description).toContain("Read a file");
  expect((t[0]!.inputSchema as any).properties.path).toBeDefined();
  expect(log.find((l) => l.url === "http://10.0.0.5:7788/tools")!.auth).toBe("Bearer pt");
  expect(await t[0]!.run({ path: "a" })).toBe('{\n  "ok": 1\n}');
  expect(log.at(-1)!.auth).toBe("Bearer pt");
});

test("pod not ready falls back to the permissive schema and call says not ready", async () => {
  handler = () => j({ error: "workspace not ready" }, 409);
  const t = await podTools("w1");
  expect(t.length).toBe(21);
  expect(t[0]!.inputSchema).toEqual({ type: "object", additionalProperties: true });
  expect(await t[0]!.run({})).toStartWith("workspace not ready: ");
});

test("a network error re-fetches the address once and retries once", async () => {
  let addr = 0;
  handler = (u) => {
    if (u.endsWith("/v1/workspaces/w1/tools")) return j({ address: `10.0.0.${++addr}:7788` });
    if (u.endsWith("/tools")) return j({ tools: [] });
    if (u.startsWith("http://10.0.0.1")) throw new TypeError("connection refused");
    return j({ done: true });
  };
  const t = await podTools("w1");
  expect(await t.find((x) => x.name === "exec")!.run({ cmd: "ls" })).toContain("done");
  expect(addr).toBe(2);
  // a second failure is not retried again
  handler = () => {
    throw new TypeError("down");
  };
  expect(await t[0]!.run({})).toContain("error");
});
