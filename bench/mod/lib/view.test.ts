import { test } from "node:test";
import assert from "node:assert/strict";
import { buildView } from "./view.ts";

// Field names corrected against the real handlers (see view.ts header comment): `state`, not
// `phase` (lowercase WsState); `services`/`service_status` at the environment's top level, with
// `service_status[].interceptedBy` (camelCase) naming the intercepting workspace by id.

test("intercepts show as svc:port on the intercepting workspace", () => {
  const v = buildView(
    [{ id: "ws-1", name: "api", state: "ready" }] as any,
    [{ id: "env-1", name: "dev", services: [{ name: "web", ports: [8080] }], service_status: [{ name: "web", interceptedBy: "ws-1" }] }] as any,
    {} as any,
  );
  assert.deepEqual(v.workspaces[0]!.intercepts, ["web:8080"]);
});

test("a session error line marks the row errored", () => {
  const v = buildView(
    [{ id: "ws-1", name: "api", state: "ready" }] as any,
    [],
    { "ws-1": { lines: ["u:x", "s:(error) boom"], busy: false, queued: [], agents: [] } } as any,
  );
  assert.equal(v.workspaces[0]!.status, "errored");
});

test("a stopped workspace is stopped whatever the session says", () => {
  const v = buildView([{ id: "ws-1", name: "api", state: "stopped" }] as any, [], {} as any);
  assert.equal(v.workspaces[0]!.status, "stopped");
});

test("doing prefers the last t: line over u:, truncated to 60 chars", () => {
  const v = buildView(
    [{ id: "ws-1", name: "api", state: "ready" }] as any,
    [],
    { "ws-1": { lines: ["u:go", "t:" + "x".repeat(80)], busy: true, queued: [], agents: [] } } as any,
  );
  assert.equal(v.workspaces[0]!.doing.length, 60);
  assert.equal(v.workspaces[0]!.status, "running");
});

test("no session yet is idle with nothing queued", () => {
  const v = buildView([{ id: "ws-1", name: "api", state: "ready" }] as any, [], {} as any);
  assert.equal(v.workspaces[0]!.status, "idle");
  assert.equal(v.workspaces[0]!.queued, 0);
});
