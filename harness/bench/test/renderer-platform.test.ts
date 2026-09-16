import { test } from "node:test";
import assert from "node:assert/strict";
import { ipcError, toEnvironment, toSnapshot, toWorkspace } from "../../src/renderer/platform.ts";

test("toWorkspace: ready is running, pins split, nothing invented", () => {
  const w = toWorkspace({ id: "ws-1", name: "api", state: "creating", packages: ["jq", "nodejs@22.1"] });
  assert.equal(w.state, "stopped");
  assert.equal(toWorkspace({ id: "x", name: "x", state: "ready", packages: [] }).state, "running");
  assert.deepEqual(w.packages, [{ name: "jq", version: "" }, { name: "nodejs", version: "22.1", pinned: true }]);
  assert.deepEqual([w.repo, w.branch, w.queue, w.ephemerals, w.files, w.changes], ["", "", [], [], [], []]);
});

test("toEnvironment: service state from status, an intercept lands on its remapped port", () => {
  const base = {
    id: "env-1", owner: "acme", name: "staging", region: "r1", state: "running", volume: "vol/acme/env-1",
    services: [{ name: "api", image: "i", ports: [8080, 9090] }, { name: "db", image: "m", ports: [] }],
    serviceStatus: [{ name: "api", ready: true, interceptedBy: "ws-1" }, { name: "db", ready: false, message: "pulling image" }],
    intercepts: [{ service: "api", workspace: "ws-1", ports: [{ service: 8080, workspace: 3000 }] }],
  };
  const e = toEnvironment(base, "acme");
  assert.equal(e.owner, "team");
  assert.equal(e.volume, "env-1");
  assert.deepEqual(e.services[0].ports, [
    { port: 8080, protocol: "tcp", intercept: { workspace: "ws-1", port: 3000 } },
    { port: 9090, protocol: "tcp", intercept: { workspace: "ws-1", port: 9090 } },
  ]);
  assert.equal(e.services[0].state, "running");
  assert.deepEqual([e.services[1].state, e.services[1].note], ["starting", "pulling image"]);
  assert.equal(toEnvironment({ ...base, state: "stopped" }, "acme").services[0].state, "stopped");
  assert.equal(toEnvironment({ ...base, state: "error" }, "acme").services[1].state, "failed");
  assert.equal(toEnvironment({ ...base, owner: "karthik", volume: undefined }, "acme").owner, "you");
});

test("toSnapshot: the message names it, a non-ready phase is the note", () => {
  const s = toSnapshot({ id: "snap-1", phase: "pending", services: 3 }, "staging");
  assert.deepEqual(s, { id: "snap-1", name: "snap-1", environment: "staging", at: "", by: "", services: 3, note: "pending" });
  assert.equal(toSnapshot({ id: "a", message: "good", phase: "ready" }, "x").name, "good");
});

test("ipcError strips Electron's remote-method wrapper", () => {
  assert.equal(ipcError(new Error("Error invoking remote method 'platform:workspaces': Error: Kloudlite answered 503")), "Kloudlite answered 503");
  assert.equal(ipcError(new Error("plain")), "plain");
});
