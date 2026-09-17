import { test } from "node:test";
import assert from "node:assert/strict";
import { pickRenderer, processes, capabilities } from "../../src/renderer/components/results/pick.ts";
import { procsOf, sessionOf } from "../../src/renderer/rows.ts";
import { onEvent, planOf } from "../../src/renderer/live.ts";

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
