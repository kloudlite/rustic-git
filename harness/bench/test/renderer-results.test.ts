import { test } from "node:test";
import assert from "node:assert/strict";
import { pickRenderer, processes, capabilities } from "../../src/renderer/components/results/pick.ts";
import { displayModel, modeLine, procsOf, sessionOf } from "../../src/renderer/rows.ts";
import { AUTO_YES, MODES, onEvent, planOf } from "../../src/renderer/live.ts";
import { grepBlock, plainBlock, readBlock } from "../../src/renderer/components/results/code.ts";
import { render as renderLine, report, toolLine } from "../../src/renderer/components/results/toolline.ts";
import { elapsed, segments, timing, verb } from "../../src/renderer/components/results/group.ts";
import { notification, spinnerMeta, summary, turnFooter, verbAt } from "../../src/renderer/components/results/summary.ts";

test("a tool's answer picks its card, and an unknown shape keeps the block", () => {
  const ws = JSON.stringify({ id: "api", name: "api", state: "running", packages: ["go@1.22"] });
  assert.deepEqual(pickRenderer("kl_workspace", ws)?.kind, "workspace");
  assert.equal(pickRenderer("kl_workspace_create", ws)?.kind, "workspace");
  // One document answered as a list of one is still one document.
  assert.equal(pickRenderer("kl_workspace", `[${ws}]`)?.kind, "workspace");
  assert.equal(pickRenderer("kl_environment", JSON.stringify({ id: "dev", services: [{ name: "db", image: "mongo:7" }] }))?.kind, "environment");
  assert.equal(pickRenderer("kl_quota", JSON.stringify({ owner: "ada", limit: { cpu: 40 }, used: { cpu: 2 } }))?.kind, "quota");
  assert.equal(pickRenderer("kl_volume_history", JSON.stringify([{ id: "snap-1" }]))?.kind, "history");
  assert.equal(pickRenderer("kl_pkg_list", JSON.stringify(["go@1.22", "ripgrep"]))?.kind, "packages");
  assert.equal(pickRenderer("kl_capabilities", "workspace:\n  kl_workspace [read] — one workspace")?.kind, "capabilities");
  assert.deepEqual(pickRenderer("kl_workspace_ask", "queued in api's session", { workspace: "api" }), { kind: "ask", data: { workspace: "api" } });
  assert.equal(pickRenderer("process", "p1 running npm run dev")?.kind, "processes");

  // Nothing forced: a shape this build does not know keeps the JSON block it always had.
  assert.equal(pickRenderer("kl_workspace", "not json"), undefined);
  assert.equal(pickRenderer("kl_whoami", JSON.stringify({ username: "ada" })), undefined);
  assert.equal(pickRenderer("kl_quota", JSON.stringify({ owner: "ada" })), undefined, "a quota without limit/used is not a quota card");
  assert.equal(pickRenderer("bash", "hello"), undefined);
  assert.equal(pickRenderer(undefined, "x"), undefined);
  assert.equal(pickRenderer("kl_workspace", undefined), undefined);
});

test("the process list and the capability list are read back from what the tools print", () => {
  assert.deepEqual(processes("p1 running npm run dev\np2 exited (exit 0) build\nnothing here"), [
    { id: "p1", state: "running", cmd: "npm run dev" },
    { id: "p2", state: "exited 0", cmd: "build" },
  ]);
  assert.deepEqual(capabilities(["this machine (its own files and shell, nowhere else):", "  read, write, edit", "workspace:", "  kl_workspace [read] — one workspace in full", "  kl_workspace_delete [destroy] — delete it", "anything not listed is not something you can do — say so."]. join("\n")), [
    { group: "this machine (its own files and shell, nowhere else)", tools: [{ name: "read, write, edit", effect: "", summary: "" }] },
    { group: "workspace", tools: [{ name: "kl_workspace", effect: "read", summary: "one workspace in full" }, { name: "kl_workspace_delete", effect: "destroy", summary: "delete it" }] },
  ]);
});

test("the processes panel shows one session's, and a tab names its own session", () => {
  const rows = [{ id: "p1", session: "bench" }, { id: "p2", session: "w-api" }, { id: "p3" }];
  assert.deepEqual(procsOf(rows, "bench").map((p) => p.id), ["p1"]);
  assert.deepEqual(procsOf(rows, "w-api").map((p) => p.id), ["p2"], "a workspace's dev server is not the bench's");
  assert.deepEqual(procsOf(rows, "nope"), []);
  assert.equal(sessionOf({ kind: "bench" }), "bench");
  assert.equal(sessionOf({ kind: "session", id: "s-2" }), "s-2");
  assert.equal(sessionOf({ kind: "workspace", id: "api" }), "w-api");
  assert.equal(sessionOf({ kind: "ephemeral", id: "api-eph-1" }), "e-api-eph-1");
});

test("a plan event fills the panel, with the doing item and the reason for a later one", () => {
  onEvent({
    type: "plan",
    session: "s-plan",
    items: [
      { text: "clone the repo", state: "done" },
      { text: "add the endpoint", state: "doing" },
      { text: "open a pull request", state: "later", why: "the API is not merged yet" },
      { text: "write the tests", state: "todo" },
    ],
  });
  // The panel's own four states — the tree already draws these, so a plan needs no second shape.
  assert.deepEqual(planOf("s-plan").map((t) => [t.text, t.state, t.note]), [
    ["clone the repo", "done", undefined],
    ["add the endpoint", "active", undefined],
    ["open a pull request", "blocked", "the API is not merged yet"],
    ["write the tests", "pending", undefined],
  ]);
  assert.deepEqual(planOf("nobody"), [], "a session with no plan has no plan, not a stale one");
});

test("a read result is parsed into rows: one gutter, the real numbers, the trailer as a footer", () => {
  // What the tool server prints: its OWN line numbers. The viewer used to add a second gutter, so
  // the person saw "1 1 import fs" with the first line out of step (owner's screenshot).
  const out = ['   1\timport fs from "node:fs";', "   2\t", "   3\texport function main() {", "[50 lines in all; page with offset]"].join("\n");
  const b = readBlock(out);
  assert.deepEqual(b.lines, [
    { n: 1, text: 'import fs from "node:fs";' },
    { n: 2, text: "" },
    { n: 3, text: "export function main() {" },
  ]);
  assert.equal(b.footer, "50 lines in all \u00b7 showing 1\u20133");

  // An offset page starts where it actually starts, not at 1.
  const page = readBlock(["  40\tconst x = 1;", "  41\tconst y = 2;", "[50 lines in all; page with offset]"].join("\n"));
  assert.deepEqual(page.lines.map((l) => l.n), [40, 41]);
  assert.equal(page.footer, "50 lines in all \u00b7 showing 40\u201341");

  // A whole file has no trailer and no footer; an unnumbered answer keeps its text.
  assert.deepEqual(readBlock("hello\nthere"), { lines: [{ text: "hello" }, { text: "there" }], footer: undefined });
  assert.equal(readBlock("x\n[truncated]").footer, "truncated");
});

test("grep rows split into path, line and match; terminal output loses its escape codes", () => {
  assert.deepEqual(grepBlock("src/a.ts:12:  const x = 1\nnot a match\nsrc/b.ts:3:fn main()"), [
    { path: "src/a.ts", n: 12, text: "  const x = 1" },
    { path: "src/b.ts", n: 3, text: "fn main()" },
  ]);
  // A dev server writes colour; a <pre> renders the escapes as mojibake.
  const coloured = `${String.fromCharCode(27)}[32mready${String.fromCharCode(27)}[0m in 300ms\n[exit 0]`;
  assert.deepEqual(plainBlock(coloured), { lines: [{ text: "ready in 300ms" }], footer: "exit 0" });
});

test("a tool call is one muted line: glyph, verb, argument, what came back", () => {
  const line = (tool: string, args: Record<string, unknown>, out?: string, state?: { pending?: boolean; secs?: number }) => renderLine(toolLine(tool, args, out, state));
  // The spec's own examples.
  assert.equal(line("grep", { pattern: "homepage|home.*button" }, Array(18).fill("a.ts:1: x").join("\n")), '∗ Grep "homepage|home.*button" (18 matches)');
  assert.equal(line("read", { path: "/home/kl/workspaces/api/path/to/file.tsx" }, "   1\tx"), "→ Read path/to/file.tsx (1 line)");
  assert.equal(line("bash", { command: "npm test" }, "ok\n[exit 0]"), "$ npm test (exit 0)");
  assert.equal(line("ask", { to: "svelte-frontend", task: "run the tests" }), "⇢ ask svelte-frontend: run the tests (queued)");
  assert.equal(line("ask", { to: "agent", name: "audit" }, undefined, { pending: true, secs: 12 }), "◐ Audit Task (running 12s)");
  assert.equal(line("ask", { to: "agent", name: "audit" }), "✓ Audit Task (started)");
  // `{Agent} Task — {description}` is opencode's own subagent grammar (`index.tsx:2317`).
  assert.equal(line("ask", { to: "agent", name: "audit", task: "check the routes" }), "✓ Audit Task — check the routes (started)");
  // A failure keeps its exit code, and a running command says so rather than lying about one.
  assert.equal(line("bash", { command: "npm test" }, "boom\n[exit 1]"), "$ npm test (exit 1)");
  assert.equal(line("bash", { command: "npm run dev" }, undefined, { pending: true }), "$ npm run dev (running)");
  // The always-on tools read as themselves; a platform tool falls back to its own name and subject.
  assert.equal(line("plan", { set: [1, 2, 3] }), "▤ Plan 3 steps");
  assert.equal(line("plan", { done: "clone the repo" }), "▤ Plan done: clone the repo");
  assert.equal(line("kl_workspace_create", { name: "svelte-backend" }), "~ workspace create svelte-backend");
  assert.equal(line("edit", { path: "src/a.ts", edits: [1, 2] }), "✎ Edit src/a.ts (2 edits)");
});

test("an agent's reply reads as status + one line, with the rest folded", () => {
  const r = report("[from agent audit-1] DONE_WITH_CONCERNS — 3 routes have no auth check\nsrc/a.ts:12\nsrc/b.ts:40");
  assert.deepEqual([r.status, r.head], ["DONE_WITH_CONCERNS", "3 routes have no auth check"]);
  assert.equal(r.body, "src/a.ts:12\nsrc/b.ts:40");
  // Every status is recognised, and the longest wins over its own prefix.
  assert.equal(report("DONE: it is done").status, "DONE");
  assert.equal(report("DONE_WITH_CONCERNS: hmm").status, "DONE_WITH_CONCERNS");
  assert.equal(report("BLOCKED no ssh host").status, "BLOCKED");
  assert.equal(report("NEEDS_CONTEXT which repo?").status, "NEEDS_CONTEXT");
  // An agent that ignored the contract still reads: no status, all body.
  assert.deepEqual(report("[from agent x] i had a look around\nand found nothing"), { status: undefined, head: "i had a look around", body: "and found nothing", left: undefined });
  // What it left behind is what the person will go and take.
  assert.equal(report("DONE — pushed branch fix-login").left, "fix-login");
  assert.equal(report("DONE_WITH_CONCERNS — opened pull ada/api#12").left, "ada/api#12");
  assert.equal(report("BLOCKED no ssh host").left, undefined);
});

test("a model reads as its name, and pi's status is never mistaken for one", () => {
  assert.equal(displayModel("deepseek/deepseek-v4-flash"), "DeepSeek V4 Flash");
  assert.equal(displayModel("anthropic/claude-opus-5"), "Claude Opus 5");
  // Unknown ids read better as themselves than as a guess, minus the vendor.
  assert.equal(displayModel("someone/new-model-9"), "new-model-9");
  assert.equal(displayModel("bare-model"), "bare-model");
  // pi's own status before a child is up is NOT a model (the owner read "not started" as one).
  assert.equal(displayModel("not started"), "no model");
  assert.equal(displayModel(""), "no model");
  assert.equal(displayModel(undefined), "no model");
});

test("consecutive tool calls of one turn read as one group", () => {
  const t = (tool: string, ts: number, ms?: number, pending?: true) => ({ role: "action" as const, kind: "run" as const, text: "", at: "", tool, ts, ms, pending });
  const say = (text: string) => ({ role: "assistant" as const, text, at: "" });

  // Six commands issued together are one decision, not six.
  const six = Array.from({ length: 6 }, (_, i) => t("bash", 1000 + i));
  const segs = segments([say("on it"), ...six, say("done")]);
  assert.deepEqual(segs.map((s) => s.kind), ["one", "group", "one"]);
  assert.equal((segs[1] as { rows: unknown[] }).rows.length, 6);
  assert.equal(verb(six as never), "6 shell commands");

  // One on its own stays as it was.
  assert.deepEqual(segments([say("a"), t("bash", 1), say("b")]).map((s) => s.kind), ["one", "one", "one"]);
  // A mixture is named for what it is, not for whichever came first.
  assert.equal(verb([t("bash", 1), t("read", 2)] as never), "2 tool calls");
  assert.equal(verb([t("read", 1), t("ls", 2)] as never), "2 reads");
  assert.equal(verb([t("grep", 1), t("find", 2)] as never), "2 searches");

  // The clock runs from the first start to the last end, and says so while anything is running.
  // 1000→3000 and 1200→4200: first start to last end.
  assert.deepEqual(timing([t("bash", 1000, 2000), t("bash", 1200, 3000)] as never, 9999), { running: false, ms: 3200 });
  assert.deepEqual(timing([t("bash", 1000, undefined, true), t("bash", 1200, 500)] as never, 6000), { running: true, ms: 5000 });
  assert.equal(elapsed(1400), "1.4s");
  assert.equal(elapsed(12_000), "12s");
  assert.equal(elapsed(310_000), "5m 10s");
});

test("the live summary says what is happening, then what happened", () => {
  const t = (tool: string) => ({ role: "action" as const, kind: "run" as const, text: "", at: "", tool });
  const rows = [t("read"), t("ls"), t("bash")];
  assert.equal(summary(rows as never), "Reading 1 file, listing 1 directory, running 1 shell command");
  assert.equal(summary(rows as never, true), "Read 1 file, listed 1 directory, ran 1 shell command");
  // Counts agree with their nouns, and several of a kind are one clause.
  assert.equal(summary([t("read"), t("read"), t("grep")] as never), "Reading 2 files, searching 1 search");
  assert.equal(summary([t("bash"), t("bash")] as never, true), "Ran 2 shell commands");
  assert.equal(summary([] as never), "");
  // A tool with no shape of its own still reads.
  assert.equal(summary([t("kl_workspace_create")] as never, true), "Made 1 tool call");

  // The spinner says it is alive: the verb changes with the seconds, and only known facts are shown.
  assert.notEqual(verbAt(0), verbAt(60));
  assert.equal(verbAt(0), verbAt(3), "it does not flicker every second");
  assert.equal(spinnerMeta(3), "3s");
  assert.equal(spinnerMeta(3, 123), "3s · ↓ 123 tokens");
  assert.equal(spinnerMeta(3, 1500, 1200), "3s · ↓ 1.5k tokens · thought for 1s");

  // The turn footer, and what is still running after it.
  const at = new Date("2026-09-17T15:33:00");
  assert.match(turnFooter(9000, at, 0), /^Crunched for 9s · done /);
  assert.match(turnFooter(9000, at, 1), /· 1 still running$/);
});

test("what the harness delivers reads as a row, not as something the person typed", () => {
  assert.deepEqual(notification("[from agent audit-1] DONE — 3 routes"), { verb: 'Agent "audit-1" finished', detail: "DONE — 3 routes" });
  assert.deepEqual(notification("[task svelte dev server finished: exit 1]\nboom"), { verb: 'Background command "svelte dev server" completed (exit code 1)' });
  assert.deepEqual(notification("[info from api] four of them"), { verb: "api answered", detail: "four of them" });
  assert.deepEqual(notification("[from workspace api] done: added /healthz"), { verb: "api replied", detail: "done: added /healthz" });
  assert.deepEqual(notification("[watch vite /error/]\nerror: boom"), { verb: "vite matched /error/", detail: "error: boom" });
  assert.deepEqual(notification("[harness] write the plan"), { verb: "Harness", detail: "write the plan" });
  // A person's own message is a person's own message.
  assert.equal(notification("add nats to the env"), undefined);
});

test("shift+tab cycles the three modes, and only own-file edits are answered for the person", () => {
  assert.deepEqual(MODES, ["build", "plan", "accept-edits"]);
  const next = (m: (typeof MODES)[number]) => MODES[(MODES.indexOf(m) + 1) % MODES.length];
  assert.equal(next("build"), "plan");
  assert.equal(next("plan"), "accept-edits");
  assert.equal(next("accept-edits"), "build", "and round again");
  // Accept-edits is about THIS machine's files. A platform write is a decision, not an edit.
  assert.deepEqual(AUTO_YES, ["write", "edit"]);
  for (const never of ["kl_workspace_delete", "kl_environment_service_rm", "kl_intercept", "bash", "ask"]) {
    assert.ok(!AUTO_YES.includes(never), `${never} must still ask`);
  }
});

test("the mode, the model and the level read the same everywhere", () => {
  // Two formats for one fact is what the owner saw: "✻ Accept edits · no model" in the footer and
  // "⏵⏵ accept edits on (⇧tab to cycle) · no model" in the composer.
  assert.equal(modeLine("build", "deepseek/deepseek-v4-flash", "low"), "Build · DeepSeek V4 Flash · low");
  assert.equal(modeLine("accept-edits", "deepseek/deepseek-v4-flash", "low"), "Accept edits · DeepSeek V4 Flash · low");
  assert.equal(modeLine("plan", "anthropic/claude-opus-5"), "Plan · Claude Opus 5", "the level shows only once it is known");
  // A session whose row has a model shows it; only a session with none at all says so.
  assert.equal(modeLine("build", undefined), "Build · no model");
});
