import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { ensureBench, Expired, mintSession } from "../../src/connect/bench.ts";

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
const ready = { status: 201, body: { id: "bench-k", token: "s1", gateway: "wss://g/tunnel/bench-k", expires_at: "2030" } };
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
