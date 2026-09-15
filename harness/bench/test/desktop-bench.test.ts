import { mock, test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { BadGateway, ensureBench, Expired, keepToolToken, listTeams, mintSession, mintToolToken, revokeLogin } from "../../src/connect/bench.ts";

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
  const s = await stub({ "GET /v1/bench/teams": [{ status: 200, body: [{ slug: "kay", name: "Personal", region: "r9", personal: true }, { slug: "acme", name: "Acme", region: "r1", personal: false }, { slug: "fresh", name: "Fresh", region: "" }] }] });
  try {
    assert.deepEqual(await listTeams(s.api, "tok"), [
      { slug: "kay", name: "Personal", region: "r9", personal: true },
      { slug: "acme", name: "Acme", region: "r1", personal: false },
      { slug: "fresh", name: "Fresh", region: "", personal: false },
    ]);
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

const TOOL = "POST /v1/bench/tool-token?team=acme";
const DROP = "DELETE /v1/bench/tool-token?team=acme";
const settle = () => new Promise((r) => setImmediate(r)).then(() => new Promise((r) => setTimeout(r, 30)));

test("connect mints a tool token after the bench is ensured", async () => {
  const s = await stub({ [SESSION]: [ready], [TOOL]: [{ status: 204 }] });
  try {
    await ensureBench(s.api, "tok", "acme", () => undefined, fast);
    await mintToolToken(s.api, "tok", "acme");
    assert.deepEqual(s.calls, [SESSION, TOOL]);
    assert.equal(s.auth[1], "Bearer tok");
  } finally {
    s.close();
  }
});

test("a tool token refusal carries the api's sentence and status", async () => {
  const s = await stub({ [TOOL]: [{ status: 409, body: { error: "bench is stopped; start it" } }] });
  try {
    await assert.rejects(mintToolToken(s.api, "tok", "acme"), (e: Error & { status?: number }) => e.message === "bench is stopped; start it" && e.status === 409);
  } finally {
    s.close();
  }
});

test("the tool token is renewed every five minutes and stops on disconnect", async () => {
  const s = await stub({ [TOOL]: [{ status: 204 }] });
  mock.timers.enable({ apis: ["setInterval"] });
  try {
    const stop = keepToolToken(s.api, "tok", "acme", () => assert.fail("not expired"));
    mock.timers.tick(5 * 60_000 - 1);
    await settle();
    assert.deepEqual(s.calls, []);
    mock.timers.tick(1);
    await settle();
    mock.timers.tick(5 * 60_000);
    await settle();
    assert.deepEqual(s.calls, [TOOL, TOOL]);
    stop();
    mock.timers.tick(15 * 60_000);
    await settle();
    assert.equal(s.calls.length, 2);
  } finally {
    mock.timers.reset();
    s.close();
  }
});

test("a renew answered 401 expires the login", async () => {
  const s = await stub({ [TOOL]: [{ status: 401 }] });
  mock.timers.enable({ apis: ["setInterval"] });
  let expired = 0;
  try {
    keepToolToken(s.api, "tok", "acme", () => void expired++);
    mock.timers.tick(5 * 60_000);
    await settle();
    assert.equal(expired, 1);
    mock.timers.tick(5 * 60_000);
    await settle();
    assert.equal(s.calls.length, 1); // the beat stops with the login
  } finally {
    mock.timers.reset();
    s.close();
  }
});

test("a renew answered 409 stops the beat instead of retrying", async () => {
  const s = await stub({ [TOOL]: [{ status: 409, body: { error: "bench is stopped; start it" } }] });
  mock.timers.enable({ apis: ["setInterval"] });
  const err = mock.method(console, "error", () => undefined);
  try {
    keepToolToken(s.api, "tok", "acme", () => assert.fail("not expired"));
    mock.timers.tick(5 * 60_000);
    await settle();
    mock.timers.tick(5 * 60_000);
    await settle();
    assert.equal(s.calls.length, 1);
  } finally {
    err.mock.restore();
    mock.timers.reset();
    s.close();
  }
});

test("sign-out deletes the tool token before revoking the login", async () => {
  const s = await stub({ [DROP]: [{ status: 204 }], "DELETE /v1/cli/tokens/j1": [{ status: 204 }] });
  try {
    await revokeLogin(s.api, "tok", "acme", "j1");
    assert.deepEqual(s.calls, [DROP, "DELETE /v1/cli/tokens/j1"]);
  } finally {
    s.close();
  }
});

test("sign-out still revokes the login when the tool token delete fails", async () => {
  const s = await stub({ [DROP]: [{ status: 500 }], "DELETE /v1/cli/tokens/j1": [{ status: 204 }] });
  try {
    await revokeLogin(s.api, "tok", "acme", "j1");
    assert.deepEqual(s.calls, [DROP, "DELETE /v1/cli/tokens/j1"]);
  } finally {
    s.close();
  }
});
