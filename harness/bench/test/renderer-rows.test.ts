import { test } from "node:test";
import assert from "node:assert/strict";
import { argLine, benchSessions, displayModel, modeLine, modeParts, modelOfThread, noteModelNames, pickerRows, turnMeta, cloneLabel, exchangeText, inFlightItems, nestWorkspaces, procLabel, procName, procState, procsOf, proposalHeader } from "../../src/renderer/rows.ts";

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

test("an exchange row reads as who and what, never as a tool call's JSON", () => {
  assert.equal(exchangeText("[ask ask-3 from karthik] run the tests"), "run the tests");
  assert.equal(exchangeText("[reply ask-3] DONE — it passes"), "DONE — it passes");
  // A platform call is no longer an exchange at all; if one reaches the log it still reads.
  assert.equal(exchangeText('kl_workspace_create {"name":"backend-rust","packages":["rust"]}'), "Workspace create");
  // Long tasks are one line.
  assert.equal(exchangeText("first line\nsecond line"), "first line");
});

test("a process row gets its title at render, whatever the ledger stored", () => {
  // A row written before the tool server titled them: the command IS the name.
  assert.equal(
    procName({ name: "cd /home/kl/workspaces/svelte-frontend && npm run dev", command: "cd /home/kl/workspaces/svelte-frontend && npm run dev" }),
    "svelte-frontend: npm run dev",
  );
  // A real title is kept.
  assert.equal(procName({ name: "dev server", command: "npm run dev" }), "dev server");
  // No title at all: the command, first line only.
  assert.equal(procName({ command: "cargo watch -x test\n" }), "cargo watch -x test");
  assert.equal(procName({ command: "" }), "");
});

/**
 * Spec §1.3: `deepseek/deepseek-reasoner · thinking high · effort max`. A segment that does not
 * apply is ABSENT, never a dash — the footer says what is set and nothing about what is not.
 */
test("footer segments appear only when set", () => {
  assert.deepEqual(modeParts("build", "deepseek/deepseek-reasoner", "high", undefined), {
    mode: "Build", model: "deepseek-reasoner", provider: "DeepSeek", thinking: "thinking high",
  });
  assert.deepEqual(modeParts("build", "deepseek/deepseek-chat", undefined, "max"), {
    mode: "Build", model: "deepseek-chat", provider: "DeepSeek", effort: "effort max",
  });
  assert.deepEqual(modeParts("build", undefined), { mode: "Build", model: "no model" });
});

test("the line joins only the segments that are there", () => {
  assert.equal(modeLine("build", "deepseek/deepseek-reasoner", "high", "max"), "Build \u00b7 deepseek-reasoner \u00b7 thinking high \u00b7 effort max");
  assert.equal(modeLine("plan", "anthropic/claude-opus-5"), "Plan \u00b7 Claude Opus 5");
});

/**
 * The footer read `claude-fable-5-1` seconds after the owner picked a DeepSeek model on the fleet:
 * Chat fell back to `props.machine.model`, which is the DEMO FIXTURE (`MACHINE.model`). The chain
 * is the session's row, then the bench default, and then NOTHING — a fixture is never a fact about
 * what is answering.
 */
test("an unknown model is no model, never the fixture", () => {
  assert.equal(modelOfThread("deepseek/deepseek-reasoner", "deepseek/deepseek-chat"), "deepseek/deepseek-reasoner");
  assert.equal(modelOfThread(undefined, "deepseek/deepseek-chat"), "deepseek/deepseek-chat");
  assert.equal(modelOfThread(undefined, undefined), undefined);
  assert.equal(displayModel(modelOfThread(undefined, undefined)), "no model");
});

/** The owner wants the readable name: pi's own catalogue beats the id it was picked by. */
test("a picked model renders by the name pi gave it", () => {
  assert.equal(displayModel("deepseek/deepseek-reasoner"), "deepseek-reasoner");
  noteModelNames([{ id: "deepseek/deepseek-reasoner", name: "DeepSeek Reasoner" }]);
  assert.equal(displayModel("deepseek/deepseek-reasoner"), "DeepSeek Reasoner");
  assert.deepEqual(modeParts("build", "deepseek/deepseek-reasoner", "high"), {
    mode: "Build", model: "DeepSeek Reasoner", provider: "DeepSeek", thinking: "thinking high",
  });
});

/**
 * A turn's footer is its OWN: it used to print the live line, so every old message changed the
 * moment the model changed (owner, on the fleet). A turn with no stamp shows what it has and
 * never falls back to what is running now.
 */
test("a turn's footer is stamped, not live", () => {
  assert.equal(
    turnMeta({ mode: "build", model: "deepseek/deepseek-v4-flash", effort: "low", duration: "Crunched for 9s" }),
    "Build \u00b7 DeepSeek V4 Flash \u00b7 DeepSeek \u00b7 effort low \u00b7 Crunched for 9s",
  );
  // An older message: mode and duration only, and NOT the live model.
  assert.equal(turnMeta({ mode: "build", duration: "Crunched for 2s" }), "Build \u00b7 Crunched for 2s");
  assert.equal(turnMeta({}), "");
  assert.equal(turnMeta({ mode: "build", interrupted: true }), "Build \u00b7 Interrupted");
});

/** The provider is its own segment; joined onto the name it read as the model said twice. */
test("the model name is not doubled by its provider", () => {
  assert.ok(!turnMeta({ model: "deepseek/deepseek-v4-flash" }).includes("Flash DeepSeek"));
  assert.ok(!modeLine("build", "deepseek/deepseek-v4-flash").includes("Flash DeepSeek"));
});

const PROVIDERS = [
  { id: "deepseek", label: "DeepSeek", wired: true, models: [{ id: "deepseek-chat", name: "DeepSeek Chat" }, { id: "deepseek-reasoner", name: "DeepSeek Reasoner" }] },
  { id: "anthropic", label: "Anthropic", wired: false, models: [{ id: "claude-opus-5", name: "Claude Opus 5" }] },
  { id: "groq", label: "Groq", wired: true, models: [] },
];

/** Owner: "why are you showing so many non configured. show only configured." */
test("only a configured provider with models is offered", () => {
  const rows = pickerRows(PROVIDERS);
  assert.deepEqual(rows.filter((r) => r.kind === "header").map((r) => (r as { label: string }).label), ["DeepSeek"]);
  assert.ok(!rows.some((r) => r.kind === "model" && r.id.startsWith("anthropic/")), "an unwired provider is not offered");
  assert.equal(pickerRows([]).length, 0, "nothing configured is an empty list, and the dialog says so");
});

/** The filter narrows models across providers, and a header left with none disappears. */
test("filter drops empty headers", () => {
  assert.deepEqual(pickerRows(PROVIDERS, "reasoner").map((r) => (r.kind === "header" ? `#${r.label}` : r.id)), ["#DeepSeek", "deepseek/deepseek-reasoner"]);
  assert.deepEqual(pickerRows(PROVIDERS, "opus"), [], "every model gone takes its header with it");
});

/** A header is a label, not a choice: the cursor can only ever rest on a model. */
test("the cursor never lands on a header", () => {
  const rows = pickerRows(PROVIDERS);
  const selectable = rows.flatMap((r, i) => (r.kind === "model" ? [i] : []));
  assert.ok(selectable.length > 0);
  for (const i of selectable) assert.equal(rows[i].kind, "model");
  assert.ok(!selectable.includes(0), "row 0 is the DeepSeek header");
});
