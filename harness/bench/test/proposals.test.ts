import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
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
  for (const yes of ["kl_workspace_create", "kl_workspace_delete", "kl_environment_service_rm", "kl_intercept", "kl_volume_delete", "kl_pull_merge", "kl_repo_create"]) assert.equal(gated(yes), true, yes);
  // A message to another session is not a change, and this machine's own packages are its own.
  for (const no of ["kl_workspace_ask", "kl_pkg_add", "kl_pkg_rm", "kl_workspaces", "kl_capabilities"]) assert.equal(gated(no), false, no);
  assert.equal(question("kl_workspace_create", { name: "svelte-backend", region: "centralindia-k3s", packages: ["nodejs", "go"] }), "Create workspace svelte-backend in centralindia-k3s with nodejs, go");
  assert.equal(question("kl_intercept", { id: "dev", service: "api", workspace: "w1" }), "Deliver api traffic in dev to workspace w1");
  assert.equal(question("kl_pull_create", { repo: "ada/api", head: "fix", base: "main", title: "stop the churn" }), "Open a pull request on ada/api: fix → main — stop the churn");
});

test("yes runs it, no declines it, and an unanswered question is a no", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-prop-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  const wait = (id: string, cap = 600_000) => fetch(`${base}/proposals/${id}/wait?cap=${cap}`).then((r) => r.json() as Promise<{ answer: string }>);
  try {
    await bench.start();
    const session = bench.sessions.all().find((s) => !s.archived)!.id;

    // The question reaches the desktop as an event, and is listed for a window that opened late.
    const seen: any[] = [];
    bench.onEvent((ev) => ev.type === "proposal" && seen.push(ev.row));
    propose(bench, session, "p-1");
    assert.equal(seen[0].summary, "Delete workspace api; its snapshots stay on the volume");
    assert.deepEqual((await (await fetch(`${base}/proposals`)).json()).map((p: any) => p.id), ["p-1"]);

    // Yes.
    const yes = wait("p-1");
    await until(() => true, 100, "");
    const answered = await (await fetch(`${base}/proposals/p-1`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "yes" }) })).json();
    assert.deepEqual(answered, { id: "p-1", answer: "yes" });
    assert.deepEqual(await yes, { answer: "yes" });
    assert.deepEqual(await (await fetch(`${base}/proposals`)).json(), [], "an answered question is not open");

    // No.
    propose(bench, session, "p-2");
    const no = wait("p-2");
    await fetch(`${base}/proposals/p-2`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "no" }) });
    assert.deepEqual(await no, { answer: "no" });

    // Unanswered at the cap: a no, because a change nobody agreed to must not happen.
    propose(bench, session, "p-3");
    assert.deepEqual(await wait("p-3", 30), { answer: "no" });

    // A question nobody asked, and an answer that is neither yes nor no.
    assert.deepEqual(await wait("p-nope"), { answer: "no" });
    assert.equal((await fetch(`${base}/proposals/p-2`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ answer: "maybe" }) })).status, 400);
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
    propose(bench, bench.sessions.all().find((s) => !s.archived)!.id, "p-9");
    const ac = new AbortController();
    const dropped = fetch(`${base}/proposals/p-9/wait`, { signal: ac.signal }).catch((e: Error) => e.name);
    ac.abort();
    assert.equal(await dropped, "AbortError");
    // Still unanswered: the abort ended one wait, it did not decide anything.
    assert.deepEqual((await (await fetch(`${base}/proposals`)).json()).map((p: any) => p.id), ["p-9"]);
  } finally {
    await srv.close();
    await bench.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
