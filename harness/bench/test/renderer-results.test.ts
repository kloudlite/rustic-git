import { test } from "node:test";
import assert from "node:assert/strict";
import { pickRenderer, processes, capabilities } from "../../src/renderer/components/results/pick.ts";
import { displayModel, procsOf, sessionOf } from "../../src/renderer/rows.ts";
import { onEvent, planOf } from "../../src/renderer/live.ts";
import { grepBlock, plainBlock, readBlock } from "../../src/renderer/components/results/code.ts";
import { render as renderLine, report, toolLine } from "../../src/renderer/components/results/toolline.ts";

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
  assert.equal(line("ask", { to: "agent", name: "audit" }, undefined, { pending: true, secs: 12 }), "◐ Agent — audit (running 12s)");
  assert.equal(line("ask", { to: "agent", name: "audit" }), "✓ Agent — audit (started)");
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
  assert.deepEqual(report("[from agent x] i had a look around\nand found nothing"), { status: undefined, head: "i had a look around", body: "and found nothing" });
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
