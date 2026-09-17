import { test } from "node:test";
import assert from "node:assert/strict";
import { benchSessions, inFlightItems, nestWorkspaces, procLabel, procState } from "../../src/renderer/rows.ts";

test("benchSessions lists bench sessions only", () => {
  const rows = [
    { id: "s-1", name: "a", seq: 1 },
    { id: "w-api", name: "api", seq: 0, kind: "workspace" },
    { id: "s-2", name: "b", seq: 2, kind: "bench" },
    { id: "e-x", name: "x", seq: 0, kind: "ephemeral" },
  ];
  assert.deepEqual(benchSessions(rows).map((r) => r.id), ["s-1", "s-2"]);
});

test("openRoute and openNote: only a workspace tab opens, and a clash as a note", async () => {
  const { openRoute, openNote } = await import("../../src/renderer/rows.ts");
  assert.equal(openRoute("workspace", "ws-1"), "/workspaces/ws-1/session");
  assert.equal(openRoute("ephemeral", "ws-1"), undefined);
  assert.match(openNote("ephemeral x belongs to ws-2"), /^this tab cannot open as a session: ephemeral x belongs to ws-2/);
  assert.equal(openNote("not connected"), "not connected");
});

test("a lost process is lost, not running or exited", () => {
  assert.equal(procState({ lost: true, ended: 5 }), "lost");
  assert.equal(procLabel({ lost: true, ended: 5 }), "lost");
  assert.equal(procState({}), "running");
  assert.equal(procState({ ended: 1, code: 0 }), "done");
  assert.equal(procState({ ended: 1, code: 2 }), "failed");
  assert.equal(procLabel({ ended: 1, code: 2 }), "exited 2");
});

test("inFlightItems reads the bench's delete refusal", () => {
  assert.deepEqual(inFlightItems("in flight: Bash sleep 9, process web"), ["Bash sleep 9", "process web"]);
  assert.equal(inFlightItems("no session s-9"), undefined);
});

test("refusal: offline and sessionless refuse everything; unwritable refuses only writes", async () => {
  const { refusal } = await import("../../src/renderer/rows.ts");
  const up = { session: "s-1", connected: true, writable: { ok: true } };
  assert.equal(refusal({ type: "prompt" }, up), undefined);
  assert.match(refusal({ type: "abort" }, { ...up, connected: false })!, /not connected/);
  assert.match(refusal({ type: "get_state" }, { ...up, session: "" })!, /no session/);
  const ro = { ...up, writable: { ok: false, reason: "disk full" } };
  for (const type of ["prompt", "new_session", "compact", "set_model"]) assert.match(refusal({ type }, ro)!, /disk full/);
  assert.equal(refusal({ type: "abort" }, ro), undefined);
});

/**
 * A clone belongs under the workspace it was cut from. The api lists both flat and the owner saw
 * `ws-30b60ec83f5ff77f-eph-5m3k1p` sitting at the top level by its id (2026-09-17) — the deferred
 * clone path names a clone after the parent's ID, a person's after its NAME, so both must match.
 */
test("workspaces nest their clones, by the parent's id or its name", () => {
  const rows = [
    { id: "ws-30b60ec83f5ff77f", name: "svelte-frontend" },
    { id: "ws-30b60ec83f5ff77f-eph-5m3k1p", name: "ws-30b60ec83f5ff77f-eph-5m3k1p" },
    { id: "ws-api", name: "api" },
    { id: "ws-api-clone", name: "api-eph-9q2z" },
    { id: "ws-alone", name: "alone" },
  ];
  const tree = nestWorkspaces(rows, { "ws-30b60ec83f5ff77f-eph-5m3k1p": "audit-1" });
  assert.deepEqual(tree.map((n) => n.row.id), ["ws-30b60ec83f5ff77f", "ws-api", "ws-alone"], "only real machines at the top");
  assert.deepEqual(tree[0].clones.map((c) => c.row.id), ["ws-30b60ec83f5ff77f-eph-5m3k1p"], "matched by the parent's id");
  assert.equal(tree[0].clones[0].agent, "audit-1", "and labelled by the agent working in it");
  assert.deepEqual(tree[1].clones.map((c) => c.row.id), ["ws-api-clone"], "matched by the parent's name");
  assert.equal(tree[2].clones.length, 0);
  // An orphan clone is still a machine, not a row that disappears.
  const orphan = nestWorkspaces([{ id: "ws-x-eph-1", name: "gone-eph-1" }]);
  assert.deepEqual(orphan.map((n) => n.row.id), ["ws-x-eph-1"]);
});
