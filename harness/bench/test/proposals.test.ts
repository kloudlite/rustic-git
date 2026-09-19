import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench, withNames } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./fake-pi.ts";
import { until } from "./wait.ts";
import { gated, question } from "../../pi/catalog.ts";

/**
 * Nothing changes on somebody's platform until they say yes (spec §9). The extension publishes the
 * question, the bench holds the tool call, a person answers in the desktop.
 */
const propose = (bench: Bench, session: string, id: string, tool = "kl_workspace_delete") =>
  (bench as any).foldRow(session, {
    type: "extension_ui_request",
    method: "setWidget",
    widgetKey: "harness:proposal",
    widgetLines: [JSON.stringify({ id, tool, args: { id: "api" }, summary: question(tool, { id: "api" }) })],
  });

test("the catalogue says which calls are asked about first", () => {
  for (const yes of ["kl_workspace_create", "kl_workspace_delete", "kl_environment_service_rm", "kl_intercept", "kl_pull_merge", "kl_repo_create"]) assert.equal(gated(yes), true, yes);
  // A message is not a platform change; package mutations are changing a workspace and are gated.
  for (const no of ["kl_workspace_ask", "kl_workspaces", "kl_capabilities", "kl_workspace_snapshots"]) assert.equal(gated(no), false, no);
  for (const yes of ["kl_pkg_add", "kl_pkg_rm"]) assert.equal(gated(yes), true, yes);
  // No region and no owner: a person types neither, so the question does not print them either.
  assert.equal(question("kl_workspace_create", { name: "svelte-backend", packages: ["nodejs", "go"] }), "Create workspace svelte-backend with nodejs, go");
  assert.equal(question("kl_intercept", { id: "dev", service: "api", workspace: "w1" }), "Deliver api traffic in dev to workspace w1");
  assert.equal(question("kl_pull_create", { repo: "ada/api", head: "fix", base: "main", title: "stop the churn" }), "Open a pull request on ada/api: fix → main — stop the churn");
});

test("yes runs it, no declines it, and an unanswered question is a no", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-prop-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  // The extension waits on the id it minted AND says which session it is: the same tool-call id can
  // be open in two sessions, and the bench will not guess between them (R-D27).
  const wait = (id: string, cap = 600_000, who?: string) =>
    fetch(`${base}/proposals/${encodeURIComponent(id)}/wait?cap=${cap}${who ? `&session=${encodeURIComponent(who)}` : ""}`).then((r) => r.json() as Promise<{ answer: string }>);
  try {
    await bench.start();
    const session = bench.sessions.all().find((s) => !s.archived)!.id;

    // The question reaches the desktop as an event, and is listed for a window that opened late.
    const seen: any[] = [];
    bench.onEvent((ev) => ev.type === "proposal" && seen.push(ev.row));
    propose(bench, session, "p-1");
    assert.equal(seen[0].summary, "Delete workspace api; its snapshots stay on the volume");
    // A card is addressed by its own key — the session and the child's id — so two sessions raising
    // the same id are two cards.
    const keyOf = (raw: string) => `${session}.${raw}`;
    assert.deepEqual((await (await fetch(`${base}/proposals`)).json()).map((p: any) => p.id), [keyOf("p-1")]);

    // Yes.
    const yes = wait(keyOf("p-1"), 600_000, session);
    await until(() => true, 100, "");
    const answered = await (await fetch(`${base}/proposals/${keyOf("p-1")}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "yes" }) })).json();
    assert.deepEqual(answered, { id: keyOf("p-1"), answer: "yes" });
    assert.deepEqual(await yes, { answer: "yes" });
    assert.deepEqual(await (await fetch(`${base}/proposals`)).json(), [], "an answered question is not open");

    // No.
    propose(bench, session, "p-2");
    const no = wait(keyOf("p-2"), 600_000, session);
    await fetch(`${base}/proposals/${keyOf("p-2")}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "no" }) });
    assert.deepEqual(await no, { answer: "no" });

    // Unanswered at the cap: a no, because a change nobody agreed to must not happen.
    propose(bench, session, "p-3");
    assert.deepEqual(await wait(keyOf("p-3"), 30, session), { answer: "no" });

    // A question nobody asked.
    assert.deepEqual(await wait(keyOf("p-nope"), 600_000, session), { answer: "no" });
    // An empty answer is not an answer; a `question` tool's answer is the person's own words, so
    // anything they actually said is taken (§17.6).
    assert.equal((await fetch(`${base}/proposals/${keyOf("p-2")}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "" }) })).status, 400);
  } finally {
    await srv.close();
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a waiting client that goes away stops waiting, and the question stays open for another", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-prop-x-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  try {
    await bench.start();
    const only = bench.sessions.all().find((s) => !s.archived)!.id;
    const keyOf = (raw: string) => `${only}.${raw}`;
    propose(bench, only, "p-9");
    const ac = new AbortController();
    const dropped = fetch(`${base}/proposals/${keyOf("p-9")}/wait`, { signal: ac.signal }).catch((e: Error) => e.name);
    ac.abort();
    assert.equal(await dropped, "AbortError");
    // Still unanswered: the abort ended one wait, it did not decide anything.
    assert.deepEqual((await (await fetch(`${base}/proposals`)).json()).map((p: any) => p.id), [keyOf("p-9")]);
  } finally {
    await srv.close();
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("a question to the person carries its own options, and the answer is the tool's result", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-question-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  const srv = await serve(bench, 0);
  const t = { bench, base: `http://127.0.0.1:${srv.port}`, down: async () => (await srv.close(), await bench.stop(), fs.rmSync(dir, { recursive: true, force: true })) };
  try {
    await bench.start();
    const session = t.bench.sessions.all().find((s) => !s.archived)!.id;
    const asked: any[] = [];
    t.bench.onEvent((ev) => ev.type === "proposal" && asked.push(ev.row));
    // What `question` publishes: the same channel a proposal uses, with the options the model wrote.
    (t.bench as any).foldRow(session, {
      type: "extension_ui_request",
      method: "setWidget",
      widgetKey: "harness:proposal",
      widgetLines: [JSON.stringify({ id: "q-1", tool: "question", args: {}, summary: "Which database?", question: { header: "Storage", options: [{ label: "postgres", description: "you already run one" }, { label: "mongodb", description: "the services expect it" }] } })],
    });
    assert.equal(asked[0].tool, "question");
    assert.deepEqual((asked[0].question as { options: { label: string }[] }).options.map((o) => o.label), ["postgres", "mongodb"]);

    // The person's answer is their own words, not a yes/no, and it reaches the model as the
    // question tool's RESULT — never also as a user message. It used to be sent as a prompt too, so
    // pi wrote a `postgres` user row into the session file and every reopen replayed it beneath the
    // card that already said it (owner, on the transcript).
    const waited = fetch(`${t.base}/proposals/q-1/wait?session=${encodeURIComponent(session)}`).then((r) => r.json() as Promise<{ answer: string }>);
    await new Promise((r) => setTimeout(r, 30));
    await fetch(`${t.base}/proposals/${encodeURIComponent(`${session}.q-1`)}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "postgres" }) });
    assert.deepEqual(await waited, { answer: "postgres" });
    await new Promise((r) => setTimeout(r, 100));
    const msgs = (await t.bench.messages(session)).messages as { role?: string; content?: unknown }[];
    assert.ok(!msgs.some((m) => m.role === "user" && String(m.content) === "postgres"), "the card is the record; the answer is not a second user message");
  } finally {
    await t.down();
  }
});

/**
 * D3/D4 (api-test-report). Two children mint the same tool call id (`call_00_…`), the card was
 * keyed by that id alone, and `if (!this.proposals.has(p.id))` DROPPED the second — so s-14's card
 * carried s-13's session, the person's "yes" released the wrong tool call, and s-14's create was
 * told "declined by the person" while the workspace had in fact been made. The person was told the
 * opposite of the truth.
 */
test("two sessions raising the same tool call id get two cards", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-collide-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const a = bench.sessions.create({ model: "fake/m" }).id;
    const b = bench.sessions.create({ model: "fake/m" }).id;
    const card = (session: string) =>
      (bench as unknown as { foldRow: (id: string, ev: unknown) => void }).foldRow(session, {
        type: "extension_ui_request",
        method: "setWidget",
        widgetKey: "harness:proposal",
        widgetLines: [JSON.stringify({ id: "call_00_same", tool: "kl_workspace_create", args: { name: "t-go" }, summary: "Create workspace t-go" })],
      });
    card(a);
    card(b);
    const open = bench.openProposals();
    assert.equal(open.length, 2, "one card each: the second is not dropped into the first");
    assert.deepEqual(open.map((p) => p.session).sort(), [a, b].sort(), "and each card names the session that raised it");

    // D4: each answer releases its OWN session's tool call. The person answered yes, the workspace
    // was created, and the bench said "Declined" — because one card had absorbed both waits.
    const forA = open.find((p) => p.session === a)!;
    const forB = open.find((p) => p.session === b)!;
    assert.notEqual(forA.id, forB.id, "two cards, two ids");
    const waitingB = bench.waitProposal(forB.id, 5_000);
    bench.answerProposal(forA.id, "no");
    assert.equal(bench.answerProposal(forB.id, "yes").answer, "yes", "B's answer is B's own");
    assert.equal(await waitingB, "yes", "and B's tool call is released with it, not with A's");
    assert.equal(bench.openProposals().length, 0, "both settled");
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * R-D15. A card said "Deliver mongodb traffic in env-2193… to workspace ws-9e16…" while its
 * siblings used names. A card is read by a PERSON, so it says what they call things.
 */
test("a card says names, not raw ids", () => {
  const names = { "ws-9e16aa01bb22cc33": "frontend", "env-2193aa44": "staging" };
  assert.equal(
    withNames("Deliver mongodb traffic in env-2193aa44 to workspace ws-9e16aa01bb22cc33", names),
    "Deliver mongodb traffic in staging to workspace frontend",
  );
  // An id nobody has a name for is left alone: a wrong name is worse than an id.
  assert.equal(withNames("Write a file in ws-000000000000dead", names), "Write a file in ws-000000000000dead");
  // Nothing else in the sentence is touched.
  assert.equal(withNames("no ids here at all", names), "no ids here at all");
  assert.equal(withNames("", names), "");
});

/** The whole card — summary, preview and the question's own options — is what a person reads. */
test("the whole card is named, not only its first line", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-names-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const session = bench.sessions.all().find((s) => !s.archived)!.id;
    // A workspace session gives the bench the name for its own id.
    await bench.openWorkspace("ws-9e16aa01bb22cc33").catch(() => undefined);
    (bench as unknown as { sessions: { update: (id: string, p: unknown) => void } }).sessions.update("w-ws-9e16aa01bb22cc33", { name: "frontend" });
    (bench as unknown as { foldRow: (id: string, ev: unknown) => void }).foldRow(session, {
      type: "extension_ui_request",
      method: "setWidget",
      widgetKey: "harness:proposal",
      widgetLines: [JSON.stringify({
        id: "p-named",
        tool: "question",
        args: {},
        summary: "Send traffic to ws-9e16aa01bb22cc33",
        preview: "target: ws-9e16aa01bb22cc33",
        question: { header: "ws-9e16aa01bb22cc33", options: [{ label: "yes, ws-9e16aa01bb22cc33", description: "it runs in ws-9e16aa01bb22cc33" }] },
      })],
    });
    const card = bench.openProposals().find((p) => p.id.endsWith("p-named"))!;
    assert.match(card.summary, /frontend/, "the summary is named");
    assert.ok(!/ws-9e16/.test(JSON.stringify(card)), `no raw id anywhere in the card: ${JSON.stringify(card)}`);
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * R-D27. D3 keyed a SECOND session's card by `{session}~{id}` but left the first on the bare id,
 * and both lookups then fell back to `.find(x => x.raw === id)` — which picks whichever card it
 * meets first. With several cards open, an unrelated `yes` released someone else's tool call and
 * the person's own read as "declined". An answer addresses one card: its own key, or nothing.
 */
test("an answer never reaches another session's card", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-d27-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  try {
    await bench.start();
    const a = bench.sessions.create({ model: "fake/m" }).id;
    const b = bench.sessions.create({ model: "fake/m" }).id;
    const raise = (session: string) =>
      (bench as unknown as { foldRow: (id: string, ev: unknown) => void }).foldRow(session, {
        type: "extension_ui_request",
        method: "setWidget",
        widgetKey: "harness:proposal",
        widgetLines: [JSON.stringify({ id: "call_same", tool: "bash", args: { command: "ls" }, summary: "Run ls" })],
      });
    raise(a);
    raise(b);
    const open = bench.openProposals();
    assert.equal(open.length, 2);
    const cardA = open.find((p) => p.session === a)!;
    const cardB = open.find((p) => p.session === b)!;

    bench.answerProposal(cardA.id, "yes");
    const still = bench.openProposals();
    assert.deepEqual(still.map((p) => p.session), [b], "B's card is still waiting for its own answer");
    assert.equal(still[0].id, cardB.id);

    // The raw id alone addresses nothing: it cannot name which card is meant.
    assert.throws(() => bench.answerProposal("call_same", "yes"), /no proposal/);
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * R-D15, the ask path. Create/write/stop cards said names, but an ask's own card still read
 * "waiting for approval: Run in ws-dd6b76bc889bf35f: ls -1 *.go" — the same id, in the one line a
 * person is asked to act on.
 */
test("the ask card and its progress notes say names", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-d15b-"));
  const bench = new Bench({
    dir,
    readOnly: false,
    model: "fake/m",
    bin: FAKE,
    listWorkspaces: async () => [{ id: "ws-dd6b76bc889bf35f", name: "backend" }],
  });
  try {
    await bench.start();
    const asker = bench.sessions.all().find((s) => !s.archived)!.id;
    const a = await bench.ask("ws-dd6b76bc889bf35f", "list the go files", asker);
    await until(() => bench.exchanges.bySession(asker).some((e) => e.id === a.exchange), 5_000, "the ask recorded");

    (bench as unknown as { foldRow: (id: string, ev: unknown) => void }).foldRow(a.session, {
      type: "extension_ui_request",
      method: "setWidget",
      widgetKey: "harness:proposal",
      widgetLines: [JSON.stringify({ id: "p-ask", tool: "bash", args: { command: "ls -1 *.go" }, summary: "Run in ws-dd6b76bc889bf35f: ls -1 *.go" })],
    });

    await until(() => bench.exchanges.bySession(asker).some((e) => e.text.startsWith("waiting for approval")), 5_000, "the card note");
    const note = bench.exchanges.bySession(asker).find((e) => e.text.startsWith("waiting for approval"))!;
    assert.match(note.text, /backend/, "the ask card says the workspace's name");
    assert.ok(!/ws-dd6b/.test(note.text), `no raw id in what the person reads: ${note.text}`);
  } finally {
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
