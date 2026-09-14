import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { BadGateway, ensureBench, Expired, listTeams, mintSession } from "../../src/connect/bench.ts";

type Answer = { status: number; body?: unknown };
/** A stub api answering each route (method + path + query) from its own queue; the last answer repeats. */
async function stub(routes: Record<string, Answer[]>) {
  const calls: string[] = [];
  const auth: string[] = [];
  const bodies: string[] = [];
  const srv = http.createServer((req, res) => {
    const key = `${req.method} ${req.url}`;
    calls.push(key);
    auth.push(req.headers.authorization ?? "");
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      bodies.push(raw);
      const q = routes[key];
      const a = q && (q.length > 1 ? q.shift()! : q[0]);
      if (!a) return void res.writeHead(404, { "content-type": "application/json" }).end(JSON.stringify({ error: "no route" }));
      res.writeHead(a.status, { "content-type": "application/json" }).end(a.body === undefined ? "" : JSON.stringify(a.body));
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  return { api: `http://127.0.0.1:${(srv.address() as AddressInfo).port}`, calls, auth, bodies, close: () => srv.close() };
}
const SESSION = "POST /v1/bench/session?team=acme";
const START = "POST /v1/bench/start?team=acme";
const ready = { status: 201, body: { id: "bench-k", token: "s1", gateway: "wss://ws-r1.khost.dev/tunnel/bench-k", expires_at: "2030" } };
const fast = { sleepMs: 1, waitMs: 2000 };

test("a ready bench: one session call for the team, bearer auth", async () => {
  const s = await stub({ [SESSION]: [ready] });
  try {
    await ensureBench(s.api, "tok", "acme", () => undefined, fast);
    assert.deepEqual(s.calls, [SESSION]);
    assert.equal(s.auth[0], "Bearer tok");
  } finally {
    s.close();
  }
});

test("no bench yet: it is created in the team, then waited for", async () => {
  const steps: string[] = [];
  const s = await stub({
    [SESSION]: [{ status: 404, body: { error: "no bench" } }, { status: 202, body: { state: "starting" } }, ready],
    "POST /v1/bench": [{ status: 201, body: {} }],
  });
  try {
    await ensureBench(s.api, "tok", "acme", (x) => steps.push(x), fast);
    assert.deepEqual(s.calls, [SESSION, "POST /v1/bench", SESSION, SESSION]);
    assert.deepEqual(JSON.parse(s.bodies[1]), { team: "acme" });
    assert.ok(steps.includes("creating your bench"));
    assert.ok(steps.includes("bench is starting"));
  } finally {
    s.close();
  }
});

test("a stopped bench is started once, in the team", async () => {
  const s = await stub({
    [SESSION]: [{ status: 409, body: { error: "bench is stopped; start it" } }, ready],
    [START]: [{ status: 202 }],
  });
  try {
    await ensureBench(s.api, "tok", "acme", () => undefined, fast);
    assert.deepEqual(s.calls, [SESSION, START, SESSION]);
  } finally {
    s.close();
  }
});

test("a team slug is encoded into the query, never spliced raw", async () => {
  const s = await stub({ "POST /v1/bench/session?team=a%26b%3Dc": [ready] });
  try {
    await mintSession(s.api, "tok", "a&b=c", fast);
    assert.deepEqual(s.calls, ["POST /v1/bench/session?team=a%26b%3Dc"]);
  } finally {
    s.close();
  }
});

test("listTeams reads slug, name and region; 401 is Expired", async () => {
  const s = await stub({ "GET /v1/bench/teams": [{ status: 200, body: [{ slug: "acme", name: "Acme", region: "r1" }, { slug: "fresh", name: "Fresh", region: "" }] }] });
  try {
    assert.deepEqual(await listTeams(s.api, "tok"), [{ slug: "acme", name: "Acme", region: "r1" }, { slug: "fresh", name: "Fresh", region: "" }]);
    assert.equal(s.auth[0], "Bearer tok");
  } finally {
    s.close();
  }
  const gone = await stub({ "GET /v1/bench/teams": [{ status: 401 }] });
  try {
    await assert.rejects(listTeams(gone.api, "tok"), Expired);
  } finally {
    gone.close();
  }
});

test("a quota refusal is shown as the server said it, with no retry loop", async () => {
  const s = await stub({
    [SESSION]: [{ status: 409, body: { error: "bench is stopped; start it" } }],
    [START]: [{ status: 409, body: { error: "cpu: 40 of 40 in use; request more under Quota" } }],
  });
  try {
    await assert.rejects(ensureBench(s.api, "tok", "acme", () => undefined, fast), /cpu: 40 of 40 in use/);
    assert.equal(s.calls.filter((c) => c === START).length, 1);
  } finally {
    s.close();
  }
});

test("401 anywhere is Expired", async () => {
  const s = await stub({ [SESSION]: [{ status: 401 }] });
  try {
    await assert.rejects(ensureBench(s.api, "tok", "acme", () => undefined, fast), Expired);
    await assert.rejects(mintSession(s.api, "tok", "acme", fast), Expired);
  } finally {
    s.close();
  }
});

test("mintSession waits through waking and gives up after waitMs", async () => {
  const s = await stub({ [SESSION]: [{ status: 202, body: { state: "waking" } }, ready] });
  try {
    assert.equal((await mintSession(s.api, "tok", "acme", fast)).token, "s1");
  } finally {
    s.close();
  }
  const never = await stub({ [SESSION]: [{ status: 202, body: { state: "waking" } }] });
  try {
    await assert.rejects(mintSession(never.api, "tok", "acme", { sleepMs: 5, waitMs: 30 }), /did not start/);
  } finally {
    never.close();
  }
});

test("a gateway address that isn't ours is refused, never returned", async () => {
  const bad = async (gateway: string) => {
    const s = await stub({ [SESSION]: [{ status: 201, body: { id: "b", token: "s1", gateway, expires_at: "2030" } }] });
    try {
      await assert.rejects(mintSession(s.api, "tok", "acme", fast), BadGateway);
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
  const s = await stub({ [SESSION]: [{ status: 201, body: { id: "b", token: "s1", gateway: "ws://127.0.0.1:1/tunnel/x", expires_at: "2030" } }] });
  try {
    await assert.rejects(mintSession(s.api, "tok", "acme", fast), BadGateway);
    const session = await mintSession(s.api, "tok", "acme", { ...fast, allowLocalGateway: true });
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
    await assert.rejects(ensureBench(api, "tok", "acme", () => undefined, fast));
    await assert.rejects(listTeams(api, "tok"));
    assert.equal(hit, false);
  } finally {
    evil.close();
    foreign.close();
  }
});

test("an abort cancels the 202 wait loop instead of retrying forever", async () => {
  const s = await stub({ [SESSION]: [{ status: 202, body: { state: "waking" } }] });
  const ac = new AbortController();
  try {
    const p = mintSession(s.api, "tok", "acme", { sleepMs: 20, waitMs: 2000, signal: ac.signal });
    setTimeout(() => ac.abort(), 5);
    await assert.rejects(p);
    const seenAfterAbort = s.calls.length;
    await new Promise((r) => setTimeout(r, 60));
    assert.equal(s.calls.length, seenAfterAbort); // no further polling after abort
  } finally {
    s.close();
  }
});
