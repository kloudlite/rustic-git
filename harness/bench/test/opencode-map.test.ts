import { test } from "node:test";
import assert from "node:assert/strict";
import { CONTEXT_TOOLS, contextCounts, contextSummary, defaultOpen, genericArgs, genericLabel, isContext, toolKind, trigger } from "../../src/renderer/components/results/opencode-map.ts";
import { segments } from "../../src/renderer/components/results/group.ts";

/**
 * Our pi tools against opencode's part catalogue. The mapping is the contract's, quoted in the
 * module; this holds it to the file:line rules rather than to whatever the UI happens to do.
 */
test("every tool maps to the part opencode registers, and the rest are generic", () => {
  assert.equal(toolKind("read"), "read");
  assert.equal(toolKind("ls"), "list", "our `ls` is their `list`");
  assert.equal(toolKind("find"), "glob", "our `find` is their `glob`");
  assert.equal(toolKind("bash"), "shell", "message-part.tsx:1489 maps bash → shell");
  assert.equal(toolKind("ask"), "task", "an agent is their subagent part");
  assert.equal(toolKind("plan"), "todowrite");
  assert.equal(toolKind("kl_workspace_create"), "generic");
  assert.equal(toolKind(undefined), "generic");
});

test("the four context tools are the four the contract names", () => {
  assert.deepEqual([...CONTEXT_TOOLS].sort(), ["glob", "grep", "list", "read"]);
  for (const t of ["read", "ls", "find", "grep"]) assert.equal(isContext(t), true, t);
  for (const t of ["bash", "edit", "ask", "kl_quota"]) assert.equal(isContext(t), false, t);
});

test("the context group counts reads, searches and lists — searches being glob plus grep", () => {
  const rows = [{ tool: "read" }, { tool: "read" }, { tool: "grep" }, { tool: "find" }, { tool: "ls" }] as never;
  assert.deepEqual(contextCounts(rows), { read: 2, search: 2, list: 1 });
  assert.deepEqual(contextSummary({ read: 2, search: 2, list: 1 }), ["2 reads", "2 searches", "1 list"]);
  // Singular and plural, and nothing at all for a count of none.
  assert.deepEqual(contextSummary({ read: 1, search: 0, list: 0 }), ["1 read"]);
  assert.deepEqual(contextSummary({ read: 0, search: 1, list: 1 }), ["1 search", "1 list"]);
  assert.deepEqual(contextSummary({ read: 0, search: 0, list: 0 }), []);
});

test("a row's trigger is its title, subtitle and at most three args", () => {
  assert.deepEqual(trigger("read", { path: "/home/kl/workspaces/api/src/index.ts", offset: 40, limit: 20 }), {
    title: "Read", subtitle: "index.ts", args: ["offset=40", "limit=20"], icon: "file",
  });
  assert.deepEqual(trigger("grep", { pattern: "auth", path: "src", glob: "*.ts" }).args, ["pattern=auth", "include=*.ts"]);
  assert.equal(trigger("ls", { path: "src/lib" }).subtitle, "lib");
  assert.equal(trigger("bash", { command: "npm test" }).subtitle, "npm test");
  assert.equal(trigger("ask", { to: "agent", name: "audit" }).title, "Agent");
  assert.equal(trigger("ask", { to: "api" }).title, "Ask");
  // Anything unregistered: `Called \`{{tool}}\`` with the generic label and args.
  const g = trigger("kl_workspace_create", { name: "api", region: "r1", team: "acme", quota_gb: 20 });
  assert.equal(g.title, "Called `kl_workspace_create`");
  assert.equal(g.subtitle, "api", "the first label key wins");
  assert.equal(g.args.length <= 3, true, "at most three args");
});

test("the generic label and args follow basic-tool.tsx:304", () => {
  assert.equal(genericLabel({ description: "d", query: "q" }), "d", "description before query");
  assert.equal(genericLabel({ pattern: "p" }), "p");
  assert.equal(genericLabel({ count: 3 }), undefined, "only non-empty strings");
  // Label keys never become args; non-primitives never become args; three at most.
  assert.deepEqual(genericArgs({ path: "x", a: 1, b: true, c: "y", d: "z", e: {} }), ["a=1", "b=true", "c=y"]);
});

test("what opens by itself is what the contract says opens", () => {
  assert.equal(defaultOpen("bash", true, false), true, "a shell, when shells open");
  assert.equal(defaultOpen("bash", false, false), false);
  assert.equal(defaultOpen("edit", false, true), true, "an edit, when edits open");
  assert.equal(defaultOpen("edit", false, true, true), false, "a pure deletion stays collapsed");
  assert.equal(defaultOpen("read", true, true), false, "a read never opens itself");
});

test("consecutive context calls become one group, and a mixed run does not", () => {
  const t = (tool: string) => ({ role: "action" as const, kind: "run" as const, text: "", at: "", tool });
  const say = (text: string) => ({ role: "assistant" as const, text, at: "" });
  assert.deepEqual(segments([say("a"), t("read"), t("grep"), t("ls"), say("b")]).map((s) => s.kind), ["one", "context", "one"]);
  // One context call on its own is still the group: the trigger says "Explored 1 read".
  assert.deepEqual(segments([t("read")]).map((s) => s.kind), ["context"]);
  // A shell among them is not context work.
  assert.deepEqual(segments([t("read"), t("bash")]).map((s) => s.kind), ["group"]);
});
