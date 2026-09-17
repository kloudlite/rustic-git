import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import type { AddressInfo } from "node:net";
import { clearMyEnvironment, getEnvironment, listEnvironments, listWorkspaces, myEnvironment, segment, setMyEnvironment, volumeHistory } from "../../src/connect/platform.ts";

type Answer = { status: number; body?: unknown; raw?: string };
/** A stub api answering `GET path?query` from a fixed table; anything else is 404. */
async function stub(routes: Record<string, Answer>) {
  const calls: string[] = [];
  const auth: string[] = [];
  const srv = http.createServer((req, res) => {
    calls.push(`${req.method} ${req.url}`);
    auth.push(req.headers.authorization ?? "");
    const a = routes[`${req.method} ${req.url}`] ?? { status: 404, body: { error: "no route" } };
    res.writeHead(a.status, { "content-type": "application/json" }).end(a.raw ?? (a.body === undefined ? "" : JSON.stringify(a.body)));
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  return { api: `http://127.0.0.1:${(srv.address() as AddressInfo).port}`, calls, auth, close: () => srv.close() };
}

const WS = { id: "ws-1", owner: "karthik", team: "acme", name: "api", region: "r1", state: "ready", image: "x", placement: null, volume: null, quota_gb: 10, packages: ["jq", "nodejs@22"], repo: "acme/api", branch: "main" };
const ENV = {
  id: "env-1", owner: "acme", name: "staging", region: "r1", state: "running", placement: "n1", volume: "vol/acme/env-1",
  services: [{ name: "api", image: "acme/api:1", command: [], env: {}, mounts: [], ports: [8080] }],
  intercepts: [{ service: "api", workspace: "ws-1", ports: [{ service: 8080, workspace: 3000 }] }],
  service_status: [{ name: "api", ready: true, interceptedBy: "ws-1" }],
};

test("workspaces: the team's, with bearer auth, cut to the rendered fields", async () => {
  const s = await stub({ "GET /v1/workspaces?team=acme": { status: 200, body: [WS] } });
  try {
    const got = await listWorkspaces(s.api, "tok", "acme");
    assert.deepEqual(got, [{ id: "ws-1", name: "api", state: "ready", repo: "acme/api", branch: "main", packages: ["jq", "nodejs@22"] }]);
    assert.equal(s.auth[0], "Bearer tok");
  } finally {
    s.close();
  }
});

test("environments: list by owner and one by id, status and intercepts carried", async () => {
  const s = await stub({ "GET /v1/environments?owner=acme": { status: 200, body: [ENV] }, "GET /v1/environments/env-1": { status: 200, body: ENV } });
  try {
    const [e] = await listEnvironments(s.api, "tok", "acme");
    assert.deepEqual(e.services, [{ name: "api", image: "acme/api:1", ports: [8080] }]);
    assert.deepEqual(e.serviceStatus, [{ name: "api", ready: true, message: undefined, interceptedBy: "ws-1" }]);
    assert.equal(e.volume, "vol/acme/env-1");
    assert.deepEqual(await getEnvironment(s.api, "tok", "env-1"), e);
  } finally {
    s.close();
  }
});

test("history: rows read, and 404 (no snapshots yet) is an empty list", async () => {
  const row = { id: "snap-1", state: { kind: "environment", services: [{}, {}], quotaGb: 5 }, lineage: [], region: "", message: "known good", createdAt: "2026-09-14T00:00:00Z", parent: null, phase: "ready" };
  const s = await stub({ "GET /v1/volumes/env-1/history": { status: 200, body: [row] } });
  try {
    assert.deepEqual(await volumeHistory(s.api, "tok", "env-1"), [{ id: "snap-1", message: "known good", createdAt: "2026-09-14T00:00:00Z", phase: "ready", services: 2 }]);
    assert.deepEqual(await volumeHistory(s.api, "tok", "env-2"), []);
  } finally {
    s.close();
  }
});

test("401 is Expired (the sign-out signal); 5xx and a 404 list are plain errors", async () => {
  const s = await stub({
    "GET /v1/workspaces?team=acme": { status: 401, body: { error: "unauthorized" } },
    "GET /v1/environments?owner=acme": { status: 503, body: { error: "down" } },
  });
  try {
    await assert.rejects(listWorkspaces(s.api, "tok", "acme"), { name: "Expired" });
    await assert.rejects(listEnvironments(s.api, "tok", "acme"), /Kloudlite answered 503/);
    await assert.rejects(getEnvironment(s.api, "tok", "env-9"), /Kloudlite answered 404/);
  } finally {
    s.close();
  }
});

test("a bad shape is refused whole", async () => {
  const s = await stub({
    "GET /v1/workspaces?team=a": { status: 200, body: { items: [] } },
    "GET /v1/workspaces?team=b": { status: 200, body: [{ id: 7, name: "x", state: "ready" }] },
    "GET /v1/workspaces?team=c": { status: 200, raw: "<html>" },
    "GET /v1/environments?owner=a": { status: 200, body: [{ ...ENV, services: [{ name: "api", image: "i", ports: [70000] }] }] },
    "GET /v1/environments?owner=b": { status: 200, body: [{ ...ENV, service_status: [{ name: "api", ready: "yes" }] }] },
  });
  try {
    for (const t of ["a", "b", "c"]) await assert.rejects(listWorkspaces(s.api, "tok", t), /unreadable/);
    for (const t of ["a", "b"]) await assert.rejects(listEnvironments(s.api, "tok", t), /unreadable/);
  } finally {
    s.close();
  }
});

test("path segments: validated before any request, encoded in the path", async () => {
  const s = await stub({});
  try {
    for (const bad of ["../x", "a/b", "a b", "a&owner=b", ".", "..", "", 7 as unknown as string]) {
      await assert.rejects(getEnvironment(s.api, "tok", bad), /not a valid name/);
      await assert.rejects(volumeHistory(s.api, "tok", bad), /not a valid name/);
      await assert.rejects(listWorkspaces(s.api, "tok", bad), /not a valid name/);
    }
    assert.deepEqual(s.calls, []);
    assert.equal(segment("env-1.a_b"), "env-1.a_b");
  } finally {
    s.close();
  }
});

test("my environment: the connected team's row, set and cleared on the platform", async () => {
  const s = await stub({
    "GET /v1/me/environments": { status: 200, body: [{ team: "other", environment: "env-9" }, { team: "Acme", environment: "env-1", region: "r1" }] },
    "PUT /v1/me/environments/acme": { status: 200, body: { team: "acme", environment: "env-2", region: "r1" } },
    "DELETE /v1/me/environments/acme": { status: 204 },
  });
  try {
    assert.equal(await myEnvironment(s.api, "tok", "acme"), "env-1");
    // A team with no row of its own follows nothing, never another team's environment.
    assert.equal(await myEnvironment(s.api, "tok", "none"), undefined);
    await setMyEnvironment(s.api, "tok", "acme", "env-2");
    await clearMyEnvironment(s.api, "tok", "acme");
    assert.deepEqual(s.calls.slice(2), ["PUT /v1/me/environments/acme", "DELETE /v1/me/environments/acme"]);
    assert.equal(s.auth[2], "Bearer tok");
    await assert.rejects(setMyEnvironment(s.api, "tok", "acme", "../etc"), /not a valid name/);
  } finally {
    s.close();
  }
});

/**
 * The owner was signed out every time the api rolled or the bench pod was recreated: ANY 401 ran
 * `auth.expired()`. A 401 is not proof a login is over — a pod answering mid-roll, a single-use
 * tunnel token, a bench route a recreated pod does not know yet all answer 401 — so the decision
 * is now: ask the identity endpoint ONCE, and only a 401 there ends the login.
 *
 * `stillValid` is that rule. It is reproduced here against the same `/v1/cli/tokens` the focus
 * re-check uses, because main's own copy is not importable from a test.
 */
async function stillValid(api: string, token: string): Promise<boolean> {
  try {
    const r = await fetch(`${api}/v1/cli/tokens`, { headers: { authorization: `Bearer ${token}` }, redirect: "error", signal: AbortSignal.timeout(10_000) });
    await r.body?.cancel();
    if (r.status === 401) return false;
    if (!r.ok) return true; // not reachable is not revoked
    return true;
  } catch {
    return true;
  }
}

test("an api 401 with a valid session does not sign out", async () => {
  const s = await stub({
    "GET /v1/workspaces?team=acme": { status: 401, body: { error: "no" } },
    "GET /v1/cli/tokens": { status: 200, body: [] },
  });
  try {
    await assert.rejects(() => listWorkspaces(s.api, "tok", "acme"), (e: Error) => e.name === "Expired");
    // The 401 names its route, and never the token.
    assert.equal((await listWorkspaces(s.api, "tok", "acme").catch((e) => e)).route, "/v1/workspaces?team=acme");
    assert.equal(await stillValid(s.api, "tok"), true, "the session is good: a roll refused us, not a revoke");
  } finally {
    s.close();
  }
});

test("an api 401 with a rejected session does sign out", async () => {
  const s = await stub({
    "GET /v1/workspaces?team=acme": { status: 401, body: { error: "no" } },
    "GET /v1/cli/tokens": { status: 401, body: { error: "revoked" } },
  });
  try {
    await assert.rejects(() => listWorkspaces(s.api, "tok", "acme"), (e: Error) => e.name === "Expired");
    assert.equal(await stillValid(s.api, "tok"), false, "the identity endpoint agrees: this login is over");
  } finally {
    s.close();
  }
});

test("an unreachable identity endpoint is not a revoked login", async () => {
  // Nothing listening: unreachable must never sign anybody out.
  assert.equal(await stillValid("http://127.0.0.1:1", "tok"), true);
});

/**
 * A BENCH-route 401 never signs anybody out. `mintSession` runs on EVERY tunnel connection, not
 * once — so with `/events` reconnecting every 79–136 s the desktop minted a session that often, and
 * one 401 among them ended the login. That is the loop the api saw as a new `parent8` every ~40 s
 * while logging zero 401s of its own.
 */
test("a bench route's 401 is a refusal to retry, not an expiry", async () => {
  const s = await stub({ "POST /v1/bench/session?team=acme": { status: 401, body: { error: "no" } } });
  try {
    const { mintSession } = await import("../../src/connect/bench.ts");
    const e = await mintSession(s.api, "tok", "acme").then(() => undefined, (x: Error) => x);
    assert.ok(e, "the mint fails");
    // It still reports as Expired from the api's own call helper — what changed is what main DOES
    // with it: `benchRefused` re-mints and only checks the login, rather than ending it outright.
    assert.equal((e as Error & { route?: string }).route, "/v1/bench/session?team=acme", "the route is named for the log");
    const main = fs.readFileSync(path.resolve("src/main.ts"), "utf8");
    assert.match(main, /keepToolToken\(.*benchRefused/, "the tool-token beat refuses, it does not expire");
    assert.match(main, /benchRefused\(c, \(e as \{ route\?: string \}\)\.route/, "and so does the tunnel");
    assert.ok(!/onExpired.*auth\.expired|\(\) => auth\.expired\(\)/.test(main), "no bench path signs out directly any more");
    // Only a rejected session JWT ends the login, and only after the identity endpoint says so.
    assert.match(main, /e\.name === "Expired" && !\(await stillValid\(c\)\)/);
  } finally {
    s.close();
  }
});
