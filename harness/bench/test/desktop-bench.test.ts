import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { BadGateway, ensureBench, Expired, mintSession } from "../../src/connect/bench.ts";

type Answer = { status: number; body?: unknown };
/** A stub api answering each route from its own queue; the last answer repeats. */
async function stub(routes: Record<string, Answer[]>) {
  const calls: string[] = [];
  const auth: string[] = [];
  const srv = http.createServer((req, res) => {
    const key = `${req.method} ${req.url}`;
    calls.push(key);
    auth.push(req.headers.authorization ?? "");
    const q = routes[key];
    const a = q && (q.length > 1 ? q.shift()! : q[0]);
    if (!a) return void res.writeHead(404, { "content-type": "application/json" }).end(JSON.stringify({ error: "no route" }));
    res.writeHead(a.status, { "content-type": "application/json" }).end(a.body === undefined ? "" : JSON.stringify(a.body));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  return { api: `http://127.0.0.1:${(srv.address() as AddressInfo).port}`, calls, auth, close: () => srv.close() };
}
const ready = { status: 201, body: { id: "bench-k", token: "s1", gateway: "wss://ws-r1.khost.dev/tunnel/bench-k", expires_at: "2030" } };
const fast = { sleepMs: 1, waitMs: 2000 };

test("a ready bench: one session call, bearer auth", async () => {
  const s = await stub({ "POST /v1/bench/session": [ready] });
  try {
    await ensureBench(s.api, "tok", () => undefined, fast);
    assert.deepEqual(s.calls, ["POST /v1/bench/session"]);
    assert.equal(s.auth[0], "Bearer tok");
  } finally {
    s.close();
  }
});

test("no bench yet: it is created, then waited for", async () => {
  const steps: string[] = [];
  const s = await stub({
    "POST /v1/bench/session": [{ status: 404, body: { error: "no bench" } }, { status: 202, body: { state: "starting" } }, ready],
    "POST /v1/bench": [{ status: 201, body: {} }],
  });
  try {
    await ensureBench(s.api, "tok", (x) => steps.push(x), fast);
    assert.deepEqual(s.calls, ["POST /v1/bench/session", "POST /v1/bench", "POST /v1/bench/session", "POST /v1/bench/session"]);
    assert.ok(steps.includes("creating your bench"));
    assert.ok(steps.includes("bench is starting"));
  } finally {
    s.close();
  }
});

test("a stopped bench is started once", async () => {
  const s = await stub({
    "POST /v1/bench/session": [{ status: 409, body: { error: "bench is stopped; start it" } }, ready],
    "POST /v1/bench/start": [{ status: 202 }],
  });
  try {
    await ensureBench(s.api, "tok", () => undefined, fast);
    assert.deepEqual(s.calls, ["POST /v1/bench/session", "POST /v1/bench/start", "POST /v1/bench/session"]);
  } finally {
    s.close();
  }
});

test("a quota refusal is shown as the server said it, with no retry loop", async () => {
  const s = await stub({
    "POST /v1/bench/session": [{ status: 409, body: { error: "bench is stopped; start it" } }],
    "POST /v1/bench/start": [{ status: 409, body: { error: "cpu: 40 of 40 in use; request more under Quota" } }],
  });
  try {
    await assert.rejects(ensureBench(s.api, "tok", () => undefined, fast), /cpu: 40 of 40 in use/);
    assert.equal(s.calls.filter((c) => c === "POST /v1/bench/start").length, 1);
  } finally {
    s.close();
  }
});

test("401 anywhere is Expired", async () => {
  const s = await stub({ "POST /v1/bench/session": [{ status: 401 }] });
  try {
    await assert.rejects(ensureBench(s.api, "tok", () => undefined, fast), Expired);
    await assert.rejects(mintSession(s.api, "tok", fast), Expired);
  } finally {
    s.close();
  }
});

test("mintSession waits through waking and gives up after waitMs", async () => {
  const s = await stub({ "POST /v1/bench/session": [{ status: 202, body: { state: "waking" } }, ready] });
  try {
    assert.equal((await mintSession(s.api, "tok", fast)).token, "s1");
  } finally {
    s.close();
  }
  const never = await stub({ "POST /v1/bench/session": [{ status: 202, body: { state: "waking" } }] });
  try {
    await assert.rejects(mintSession(never.api, "tok", { sleepMs: 5, waitMs: 30 }), /did not start/);
  } finally {
    never.close();
  }
});

test("a gateway address that isn't ours is refused, never returned", async () => {
  const bad = async (gateway: string) => {
    const s = await stub({ "POST /v1/bench/session": [{ status: 201, body: { id: "b", token: "s1", gateway, expires_at: "2030" } }] });
    try {
      await assert.rejects(mintSession(s.api, "tok", fast), BadGateway);
    } finally {
      s.close();
    }
  };
  await bad("wss://evil.test/tunnel/bench-k"); // foreign host
  await bad("ws://ws-r1.khost.dev/tunnel/bench-k"); // not wss
  await bad("https://ws-r1.khost.dev/tunnel/bench-k"); // not a ws scheme at all
  await bad("wss://ws-r1.khost.dev@evil.test/tunnel/bench-k"); // userinfo trick: host is really evil.test
  await bad("wss://ws-r1.khost.dev/other/bench-k"); // wrong path
  // a local test gateway is refused unless explicitly allowed
  const s = await stub({ "POST /v1/bench/session": [{ status: 201, body: { id: "b", token: "s1", gateway: "ws://127.0.0.1:1/tunnel/x", expires_at: "2030" } }] });
  try {
    await assert.rejects(mintSession(s.api, "tok", fast), BadGateway);
    const session = await mintSession(s.api, "tok", { ...fast, allowLocalGateway: true });
    assert.equal(session.gateway, "ws://127.0.0.1:1/tunnel/x");
  } finally {
    s.close();
  }
});

test("a redirect to a foreign origin is refused, never followed", async () => {
  const foreign = http.createServer((_req, res) => res.writeHead(200).end("hit"));
  await new Promise<void>((r) => foreign.listen(0, "127.0.0.1", r));
  let hit = false;
  foreign.on("request", () => (hit = true));
  const foreignUrl = `http://127.0.0.1:${(foreign.address() as AddressInfo).port}/`;
  const evil = http.createServer((_req, res) => res.writeHead(307, { location: foreignUrl }).end());
  await new Promise<void>((r) => evil.listen(0, "127.0.0.1", r));
  const api = `http://127.0.0.1:${(evil.address() as AddressInfo).port}`;
  try {
    await assert.rejects(ensureBench(api, "tok", () => undefined, fast));
    assert.equal(hit, false);
  } finally {
    evil.close();
    foreign.close();
  }
});

test("an abort cancels the 202 wait loop instead of retrying forever", async () => {
  const s = await stub({ "POST /v1/bench/session": [{ status: 202, body: { state: "waking" } }] });
  const ac = new AbortController();
  try {
    const p = mintSession(s.api, "tok", { sleepMs: 20, waitMs: 2000, signal: ac.signal });
    setTimeout(() => ac.abort(), 5);
    await assert.rejects(p);
    const seenAfterAbort = s.calls.length;
    await new Promise((r) => setTimeout(r, 60));
    assert.equal(s.calls.length, seenAfterAbort); // no further polling after abort
  } finally {
    s.close();
  }
});
