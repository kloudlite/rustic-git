import { test } from "node:test";
import assert from "node:assert/strict";
import { argLine, benchSessions, cloneLabel, inFlightItems, nestWorkspaces, procLabel, procState, procsOf, proposalHeader } from "../../src/renderer/rows.ts";

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

/**
 * A clone row is called after the AGENT working in it, and nothing else: the owner saw
 * `└ ⬡ probe-frontend-ws-nrt…  clone ●` — the agent's name with the clone's id trailing it
 * (2026-09-17). No id fragment ever reaches the label.
 */
test("a clone is labelled by its agent, never by its id", () => {
  assert.equal(cloneLabel("probe-frontend", "ws-nrt6k2", "ws-parent"), "probe-frontend");
  assert.equal(cloneLabel("probe-frontend-ws-nrt6k2p9", "ws-nrt6k2p9", "ws-parent"), "probe-frontend", "the clone's own id comes off");
  assert.equal(cloneLabel("audit-1-eph-5m3k1p", "ws-x", "ws-parent"), "audit-1", "and so does an -eph- suffix");
  assert.equal(cloneLabel("svelte-ws-parent", "ws-x", "ws-parent"), "svelte", "and the parent's id");
  // Nothing known: the row says what it is, with no tag to repeat it.
  assert.equal(cloneLabel(undefined, "ws-x"), "clone");
  assert.equal(cloneLabel("   ", "ws-x"), "clone");
  // Whatever comes out, no hex fragment survives.
  for (const label of [cloneLabel("probe-frontend-ws-nrt6k2p9", "ws-nrt6k2p9"), cloneLabel("x-eph-9q2z", "ws-y")])
    assert.ok(!/ws-[a-z0-9]{6,}|-eph-/.test(label), label);
});

test("a proposal is titled by the tool's own verb", () => {
  assert.equal(proposalHeader("kl_workspace_create", "Create workspace test"), "Create workspace");
  assert.equal(proposalHeader("kl_environment_service_add", "Add redis"), "Add environment service");
  assert.equal(proposalHeader("edit"), "Edit");
  assert.equal(proposalHeader("bash"), "Run");
  assert.equal(proposalHeader(undefined, "Do the thing: now"), "Do the thing", "nothing known: the sentence's own head");
});

test("a proposal says what it would act on, values only", () => {
  assert.equal(argLine({ name: "new-workspace", region: "nrt", packages: ["node", "bun"] }), "new-workspace · nrt · node, bun");
  assert.equal(argLine({ name: "test", team: "kloudlite", session: "s-1" }), "test", "routing fields are not the subject");
  assert.equal(argLine({}, "Create workspace test"), "test", "nothing to show: the summary minus the verb the header already says");
});

/**
 * Processes and background tasks belong to a WORKSPACE, not to a session: every bench session
 * shares the bench's own machine, and a workspace's sessions — its thread and the agents working in
 * it — share that workspace's (owner, 2026-09-17).
 */
test("two sessions of one workspace see the same processes; another workspace sees its own", () => {
  const rows = [
    { id: "p1", session: "s-1", workspace: "bench", cmd: "npm run dev" },
    { id: "p2", session: "s-2", workspace: "bench", cmd: "tail -f log" },
    { id: "p3", session: "w-ws-api", workspace: "ws-api", cmd: "cargo watch" },
  ];
  assert.deepEqual(procsOf(rows, "s-1", "bench").map((p) => p.id), ["p1", "p2"], "a sibling session's process is this session's too");
  assert.deepEqual(procsOf(rows, "s-2", "bench").map((p) => p.id), ["p1", "p2"]);
  assert.deepEqual(procsOf(rows, "w-ws-api", "ws-api").map((p) => p.id), ["p3"], "a workspace tab sees only its own");
  // A row written before the ledger carried a workspace still belongs to the session that made it.
  assert.deepEqual(procsOf([{ id: "old", session: "s-1", cmd: "x" }], "s-1", "bench").map((p) => p.id), ["old"]);
  assert.deepEqual(procsOf([{ id: "old", session: "s-1", cmd: "x" }], "w-ws-api", "ws-api"), []);
});
