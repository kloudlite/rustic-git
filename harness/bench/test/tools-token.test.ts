import { test } from "node:test";
import assert from "node:assert/strict";
import { ToolServer, forgetToolsAuth, resolveFromApi, toolsHeader } from "../../pi/workspace-tools.ts";

/**
 * The tool server requires `Authorization: Bearer <workspace token>` on every `/tools/*`, `/fs/*`
 * and `/stream/*` call (`/healthz` is open; ttyd on 7790 is a separate server that takes none).
 * Five call sites sent no header at all. The token lives in one resolver, goes out as a header, and
 * reaches the model, the renderer, a log line and a tool result nowhere.
 */
test("a tool call carries the workspace's token", async () => {
  const seen: (string | null)[] = [];
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async (url: string | URL | Request, init?: RequestInit) => {
    seen.push(new Headers(init?.headers).get("authorization"));
    return new Response(JSON.stringify({ ok: true }), { status: 200, headers: { "content-type": "application/json" } });
  }) as typeof fetch;
  process.env.KL_TOOLS_ADDRESS = "127.0.0.1:7788";
  process.env.KL_TOOLS_TOKEN = "wt-secret";
  try {
    forgetToolsAuth("ws-1");
    const s = new ToolServer("ws-1", resolveFromApi);
    const r = await s.call({ tool: "read", args: { paths: ["a.ts"] } } as never);
    assert.equal(r.status, 200);
    assert.deepEqual(seen, ["Bearer wt-secret"], "the header is sent, once");
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.KL_TOOLS_ADDRESS;
    delete process.env.KL_TOOLS_TOKEN;
    forgetToolsAuth("ws-1");
  }
});

/** A 401 is a STALE token — the keys beat re-mints it — so it resolves once more, then gives up. */
test("a 401 re-resolves once, then answers plainly", async () => {
  const tries: (string | null)[] = [];
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async (_url: string | URL | Request, init?: RequestInit) => {
    tries.push(new Headers(init?.headers).get("authorization"));
    return new Response(JSON.stringify({ error: "no" }), { status: 401, headers: { "content-type": "application/json" } });
  }) as typeof fetch;
  process.env.KL_TOOLS_ADDRESS = "127.0.0.1:7788";
  process.env.KL_TOOLS_TOKEN = "wt-stale";
  try {
    forgetToolsAuth("ws-2");
    const s = new ToolServer("ws-2", resolveFromApi);
    const r = await s.call({ tool: "read", args: {} } as never);
    assert.equal(tries.length, 2, "one retry, not a loop");
    assert.equal(r.status, 401);
    // The refusal says what happened and never the token.
    assert.ok(!JSON.stringify(r.body).includes("wt-stale"), "no token in what a tool answers");
    assert.match(String(r.body.error), /token was not accepted/);
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.KL_TOOLS_ADDRESS;
    delete process.env.KL_TOOLS_TOKEN;
    forgetToolsAuth("ws-2");
  }
});

/** A server that does not require a token yet: no header, and the call still works. */
test("no token means no header, so both server versions work", async () => {
  const seen: (string | null)[] = [];
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async (_url: string | URL | Request, init?: RequestInit) => {
    seen.push(new Headers(init?.headers).get("authorization"));
    return new Response(JSON.stringify({ ok: true }), { status: 200, headers: { "content-type": "application/json" } });
  }) as typeof fetch;
  process.env.KL_TOOLS_ADDRESS = "127.0.0.1:7788";
  delete process.env.KL_TOOLS_TOKEN;
  try {
    forgetToolsAuth("ws-3");
    await new ToolServer("ws-3", resolveFromApi).call({ tool: "read", args: {} } as never);
    assert.deepEqual(seen, [null], "no header when there is no token to send");
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.KL_TOOLS_ADDRESS;
    forgetToolsAuth("ws-3");
  }
});

test("the header helper sends nothing without a token", () => {
  assert.deepEqual(toolsHeader({ address: "a:1" }), {});
  assert.deepEqual(toolsHeader(undefined), {});
  assert.deepEqual(toolsHeader({ address: "a:1", token: "t" }), { authorization: "Bearer t" });
});

/**
 * The header is sent from ONE place per file, not at each call site — five sites each forgot it
 * once already. A raw `fetch` at a tool-server address that does not go through the auth path is
 * the shape of that bug coming back.
 */
test("no tool-server call bypasses the auth path", async () => {
  const fs = await import("node:fs");
  const path = await import("node:path");
  const bench = fs.readFileSync(path.resolve("bench/src/bench.ts"), "utf8");
  // Every `/tools/*` the bench makes goes through `toolsFetch`, which carries the token.
  for (const m of bench.matchAll(/fetch\(`http:\/\/\$\{([a-zA-Z.]+)\}\/(tools|fs|stream)\//g)) {
    assert.match(m[1], /^at\.address$/, `a raw fetch to /${m[2]} at ${m[1]}: it must carry the token`);
  }
  assert.match(bench, /private async toolsFetch\(/, "one place sends the header");
  assert.match(bench, /authorization: `Bearer \$\{at\.token\}`/);

  const server = fs.readFileSync(path.resolve("bench/src/server.ts"), "utf8");
  assert.match(server, /const toolsFor = async \(scope: string\)/, "the proxies resolve address AND token");
  assert.match(server, /const bearer = \(t\?: string\)/);
  // ttyd is a separate server on another port and takes no token: the shell splice is untouched.
  assert.match(server, /spliceShell\(w, a, first\)/);
});
