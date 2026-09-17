import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Architecture, parseContract } from "../src/architecture.ts";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";

/**
 * The space's architecture, as one living document (§24): what runs where, what talks to what, and
 * on which endpoints. The bench holds it so it can answer those questions itself instead of asking
 * a workspace for a port it already knows.
 */
const fresh = () => new Architecture(fs.mkdtempSync(path.join(os.tmpdir(), "arch-")));

test("a section is replaced in place, and a new one is added at the end", () => {
  const a = fresh();
  a.write("# Architecture\n\npreamble\n\n## api\n\n- port 8080\n");
  a.setSection("api", "- port 9090");
  assert.match(a.read(), /## api\n\n- port 9090/);
  assert.ok(!a.read().includes("8080"), "replaced, not appended");
  assert.match(a.read(), /preamble/, "the preamble survives");
  a.setSection("web", "- port 3000");
  assert.match(a.read(), /## api[\s\S]*## web/, "a new section goes after the ones already there");
});

test("a contracts line is read into an endpoint, a shape and an owner", () => {
  assert.deepEqual(parseContract("GET /v1/workspaces — {team} → [workspace] — api"), {
    method: "GET",
    path: "/v1/workspaces",
    shape: "{team} → [workspace]",
    owner: "api",
  });
  assert.deepEqual(parseContract("- POST /orders — {items} → {id}"), { method: "POST", path: "/orders", shape: "{items} → {id}", owner: "" });
  assert.equal(parseContract("none"), undefined);
  assert.equal(parseContract(""), undefined);
  assert.equal(parseContract("nothing that looks like one"), undefined);
});

test("contracts merge by method and path, never twice", () => {
  const a = fresh();
  a.mergeContracts([
    { method: "GET", path: "/v1/workspaces", shape: "{team} → [workspace]", owner: "api" },
    { method: "POST", path: "/orders", shape: "{items} → {id}", owner: "api" },
  ]);
  assert.equal(a.contracts().length, 2);
  // The same endpoint said again UPDATES it.
  a.mergeContracts([{ method: "GET", path: "/v1/workspaces", shape: "{team, page} → [workspace]", owner: "api" }]);
  const rows = a.contracts();
  assert.equal(rows.length, 2, "two endpoints, not three");
  assert.equal(rows.find((c) => c.path === "/v1/workspaces")?.shape, "{team, page} → [workspace]");
  // And the table survives a section edit elsewhere.
  a.setSection("api", "- port 8080");
  assert.equal(a.contracts().length, 2);
});

test("the document starts from what the bench can already see", () => {
  const a = fresh();
  a.seed({
    workspaces: [{ id: "ws-1", name: "api", packages: ["bun", "postgresql"] }],
    services: [{ name: "db", image: "postgres:17", ports: [5432] }],
  });
  const doc = a.read();
  assert.match(doc, /## api\n/);
  assert.match(doc, /- workspace `ws-1`/);
  assert.match(doc, /- packages: bun, postgresql/);
  assert.match(doc, /## db \(service\)/);
  assert.match(doc, /- image: `postgres:17`/);
  assert.match(doc, /- ports: 5432/);
  assert.match(doc, /## Contracts/);
  // Seeding twice does not overwrite what somebody has written since.
  a.setSection("api", "- port 8080");
  a.seed({ workspaces: [{ id: "ws-2", name: "web" }] });
  assert.ok(!a.read().includes("## web"), "a document that exists is never re-seeded");
});

test("the bench serves the document, and takes a section through PUT", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-arch-"));
  const bench = new Bench({
    dir,
    readOnly: false,
    model: "fake/m",
    bin: FAKE,
    listWorkspaces: async () => [{ id: "ws-1", name: "api", packages: ["bun"] }],
    listServices: async () => [{ name: "db", image: "postgres:17", ports: [5432] }],
  });
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  try {
    await bench.start();
    await bench.seedArchitecture();
    const got = (await (await fetch(`${base}/architecture`)).json()) as { text: string; contracts: unknown[] };
    assert.match(got.text, /## api/);
    assert.deepEqual(got.contracts, []);

    const put = await fetch(`${base}/architecture`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ section: "api", text: "- port 8080\n- language: typescript" }),
    });
    assert.equal(put.status, 200);
    assert.match(bench.architecture.read(), /- port 8080/);

    const bad = await fetch(`${base}/architecture`, { method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify({}) });
    assert.equal(bad.status, 400);
  } finally {
    await srv.close();
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
