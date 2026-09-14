import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { getEnvironment, listEnvironments, listWorkspaces, segment, volumeHistory } from "../../src/connect/platform.ts";

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
