import { test } from "node:test";
import assert from "node:assert/strict";
import { benchSessions, inFlightItems, procLabel, procState } from "../../src/renderer/rows.ts";

test("benchSessions lists bench sessions only", () => {
  const rows = [
    { id: "s-1", name: "a", seq: 1 },
    { id: "w-api", name: "api", seq: 0, kind: "workspace" },
    { id: "s-2", name: "b", seq: 2, kind: "bench" },
    { id: "e-x", name: "x", seq: 0, kind: "ephemeral" },
  ];
  assert.deepEqual(benchSessions(rows).map((r) => r.id), ["s-1", "s-2"]);
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
