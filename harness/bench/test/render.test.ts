import { strict as assert } from "node:assert";
import { test } from "node:test";
import { TEXT_RENDER_IMMEDIATE, next, paced, step } from "../../src/renderer/components/results/paced.ts";
import { badge, editFile, patchFiles, split } from "../../src/renderer/components/results/diff.ts";
import { diagnostics, toolError, toolLine } from "../../src/renderer/components/results/toolline.ts";
import { usage } from "../../src/renderer/live.ts";
import { KEYS, LEADER, keyHint, leaderIndex, underLeader } from "../../src/renderer/keys.ts";
import { mentions, typeLabel } from "../../src/renderer/components/results/mentions.ts";

test("pacing steps by size", () => {
  assert.equal(step(10), 2);
  assert.equal(step(40), 4);
  assert.equal(step(90), 8);
  assert.equal(step(400), 100);
  assert.equal(step(10000), 256);
});

test("a tick lands on a word boundary", () => {
  // step(8) === 2, then up to eight further characters to the next boundary.
  assert.equal(next("one two ", 0), 4);
  // Nothing to snap to inside the window: the plain step stands.
  assert.equal(next("abcdefghijklmnopqrstuvwxyz", 0), 4);
});

test("only plain forward growth is paced", () => {
  const long = "x".repeat(TEXT_RENDER_IMMEDIATE + 100);
  assert.equal(paced("same", "same", true), undefined);
  assert.equal(paced(long, "", false), long, "a finished message lands whole");
  assert.equal(paced("rewritten", "other", true), "rewritten", "a rewrite lands whole");
  assert.equal(paced("ab", "abcd", true), "ab", "a shrink lands whole");
  assert.equal(paced("hello there", "hello", true), "hello there", "a small burst lands whole");
  const grown = paced(long, "", true)!;
  assert.ok(grown.length > 0 && grown.length < long.length, "a big burst is paced");
});

test("a prompt's mentions are files or agents", () => {
  assert.deepEqual(mentions("look at @bins/agent/src/lib.rs now"), [
    { type: "text", text: "look at " },
    { type: "file", text: "@bins/agent/src/lib.rs" },
    { type: "text", text: " now" },
  ]);
  assert.deepEqual(mentions("@svelte take it"), [
    { type: "agent", text: "@svelte" },
    { type: "text", text: " take it" },
  ]);
  assert.deepEqual(mentions("mail karthik@kloudlite.io"), [{ type: "text", text: "mail karthik@kloudlite.io" }]);
  assert.deepEqual(mentions("plain"), [{ type: "text", text: "plain" }]);
});

test("an attachment is named by its type", () => {
  assert.equal(typeLabel("image/png"), "Image");
  assert.equal(typeLabel("application/pdf"), "PDF");
  assert.equal(typeLabel(undefined), "File");
});

test("a unified patch is read per file", () => {
  const text = [
    "diff --git a/src/a.ts b/src/a.ts",
    "--- a/src/a.ts",
    "+++ b/src/a.ts",
    "@@ -1,3 +1,3 @@",
    " keep",
    "-old",
    "+new",
    "--- /dev/null",
    "+++ b/src/added.ts",
    "@@ -0,0 +1,2 @@",
    "+one",
    "+two",
    "--- a/src/gone.ts",
    "+++ /dev/null",
    "@@ -1 +0,0 @@",
    "-bye",
    "--- a/src/old-name.ts",
    "+++ b/src/new-name.ts",
    "@@ -1 +1 @@",
    " same",
  ].join("\n");
  const files = patchFiles(text);
  assert.deepEqual(files.map((f) => [f.path, f.type, f.additions, f.deletions]), [
    ["src/a.ts", "edit", 1, 1],
    ["src/added.ts", "add", 2, 0],
    ["src/gone.ts", "delete", 0, 1],
    ["src/new-name.ts", "move", 0, 0],
  ]);
  assert.equal(files[3].from, "src/old-name.ts");
  assert.deepEqual(badge(files[1]), { text: "Created", type: "added" });
  assert.deepEqual(badge(files[2]), { text: "Deleted", type: "removed" });
  assert.deepEqual(badge(files[3]), { text: "Moved", type: "modified" });
  assert.equal(badge(files[0]), undefined);
  // Line numbers follow the hunk header, both sides.
  const a = files[0].lines;
  assert.deepEqual(a[0], { kind: "sep", text: "@@ -1,3 +1,3 @@" });
  assert.deepEqual(a[1], { kind: "context", text: "keep", old: 1, new: 1 });
  assert.deepEqual(a[2], { kind: "del", text: "old", old: 2 });
  assert.deepEqual(a[3], { kind: "add", text: "new", new: 2 });
});

test("an edit becomes one hunk per replacement", () => {
  const f = editFile("a/b.ts", [
    { oldText: "x", newText: "y" },
    { oldText: "p\nq", newText: "r" },
  ]);
  assert.equal(f.additions, 2);
  assert.equal(f.deletions, 3);
  assert.equal(f.lines.filter((l) => l.kind === "sep").length, 1, "a separator between the two, not before the first");
  assert.deepEqual(split("a/b.ts"), { dir: "a/", name: "b.ts" });
  assert.deepEqual(split("b.ts"), { dir: "", name: "b.ts" });
});

test("a tool failure reads as title, one-word subtitle, body", () => {
  const e = toolError("edit", "Error: edit File not found: /a/b.ts\nlook elsewhere");
  assert.equal(e.title, "edit");
  assert.equal(e.subtitle, "File not found");
  assert.equal(e.body, "/a/b.ts\nlook elsewhere");
  assert.equal(toolError("bash", "it broke").subtitle, "Failed", "no `: ` means Failed");
  assert.equal(toolError(undefined, "x").title, "Tool");
});

test("diagnostics are errors only, at most three", () => {
  const out = [
    "src/a.ts:12:4: error: expected `;`",
    "src/a.ts:13:1: warning: unused",
    "src/b.ts:1:1: error: nope",
    "src/c.ts:2:2: error: nope",
    "src/d.ts:3:3: error: dropped",
  ].join("\n");
  const d = diagnostics(out);
  assert.equal(d.length, 3);
  assert.deepEqual(d[0], { path: "src/a.ts", line: 12, char: 4, message: "expected `;`" });
  assert.deepEqual(diagnostics("all fine"), []);
});

test("an unknown tool gets opencode's generic line", () => {
  const l = toolLine("clickstack_sql", { query: "select 1", limit: 10, rows: true });
  assert.equal(l.verb, "Called `clickstack_sql`");
  assert.equal(l.arg, "select 1 limit=10 rows=true");
});

test("the footer says tokens, how full the window is, and what it cost", () => {
  assert.equal(usage(12400, 1.2, 32000), "12.4K (39%) · $1.20");
  assert.equal(usage(900, 0, undefined), "900");
  assert.equal(usage(64000, undefined, 32000), "64K (100%)", "a window over its limit stops at 100");
});

test("opencode's leader is an alias layer over our own keys", () => {
  const ev = (key: string, mods: Partial<KeyboardEvent> = {}) => ({ key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, ...mods }) as KeyboardEvent;
  assert.ok(LEADER.match(ev("x", { ctrlKey: true })), "ctrl+x arms the leader");
  assert.ok(!LEADER.match(ev("x", { metaKey: true })), "⌘X is not the leader");
  assert.equal(underLeader(ev("b")), KEYS.panel);
  assert.equal(underLeader(ev("l")), KEYS.quickOpen);
  assert.equal(underLeader(ev("s")), KEYS.inspector);
  assert.equal(underLeader(ev("z")), undefined);
  assert.equal(leaderIndex(ev("3")), 2);
  assert.equal(leaderIndex(ev("0")), undefined);
  // Our own key still works, and the palette shows both.
  assert.ok(KEYS.panel.match(ev("b", { metaKey: true })));
  assert.equal(keyHint(KEYS.panel), "⌘B  ^X B");
  assert.equal(keyHint(KEYS.shell), "⌘J", "a binding with no alias reads as itself");
});
