import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { readClones, recordClone } from "@kloudlite-tui/agent";
import { LocalBackend } from "./local.ts";

const realFetch = globalThis.fetch;
const saved = { ...process.env };
afterEach(() => {
  globalThis.fetch = realFetch;
  process.env = { ...saved };
});

test("without KL_API_URL the view is unavailable with a reason, never a throw", async () => {
  delete process.env.KL_API_URL;
  process.env.KL_OWNER = "me";
  const v = await new LocalBackend().space();
  expect(v.available).toBe(false);
  expect(v.error).toContain("unavailable");
  expect(v).toMatchObject({ user: "me", workspaces: [], environments: [] });
});

test("lists the user's workspaces and the team's environments, joins clone records, survives a dead pod", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-space-"));
  writeFileSync(join(dir, "tok"), "SECRET-TOKEN\n");
  Object.assign(process.env, { KL_API_URL: "http://api", KL_TOOL_TOKEN_FILE: join(dir, "tok"), KL_OWNER: "me", KL_TEAM: "acme", KL_BENCH: "bench" });
  recordClone({ id: "c1", parent: "w1", task: "probe" });
  recordClone({ id: "gone", parent: "w1", task: "deleted elsewhere" });
  const urls: string[] = [];
  const json = (v: unknown, status = 200) => new Response(JSON.stringify(v), { status });
  globalThis.fetch = (async (u: any, init: any) => {
    const url = String(u);
    urls.push(`${init?.method ?? "GET"} ${url}`);
    expect(init.headers.authorization).toBe(url.startsWith("http://api") ? "Bearer SECRET-TOKEN" : "Bearer POD-TOKEN");
    if (url === "http://api/v1/workspaces?team=acme")
      return json([
        { id: "w1", name: "api", owner: "me", state: "ready", repo: "o/r", branch: "main" },
        { id: "c1", name: "sub", owner: "me", state: "ready" },
        { id: "w2", name: "theirs", owner: "other", state: "ready" },
        { id: "bench", name: "bench", owner: "me", state: "ready" },
        { id: "w3", name: "stopped", owner: "me", state: "stopped", attached_environment: "e1" },
      ]);
    if (url === "http://api/v1/environments?owner=acme")
      return json([
        { id: "e1", name: "prod", owner: "acme", state: "running", services: [{ name: "api", ports: [8080] }, { name: "db", ports: [5432] }], service_status: [{ name: "api", interceptedBy: "w1" }] },
        { id: "e2", name: "dev", owner: "acme", state: "running", services: [] },
      ]);
    if (url === "http://api/v1/me/environments") return json([{ team: "acme", environment: "e2" }]);
    if (url.endsWith("/v1/workspaces/w1/tools")) return json({ address: "10.0.0.1:7788", token: "POD-TOKEN" });
    if (url.endsWith("/v1/workspaces/c1/tools")) return new Response("boom", { status: 503 }); // pod down
    if (url === "http://10.0.0.1:7788/tools/process_list")
      return json({ processes: [{ id: "p1", cmd: "bun dev", state: "running", exit_code: null, failed: false }, { id: "p2", cmd: "ls", state: "exited", exit_code: 1, failed: true }] });
    if (url === "http://10.0.0.1:7788/tools/process_output") return json({ stdout: Array.from({ length: 30 }, (_, i) => `l${i}`).join("\n"), stderr: "" });
    if (url === "http://10.0.0.1:7788/fs/changes") return json({ repo: true, changes: [{ path: "a" }, { path: "b" }] });
    return json({ error: "nope" }, 404);
  }) as any;

  const v = await new LocalBackend().space();
  expect(v.available).toBe(true);
  expect(v.connected).toBe("e2");
  expect(v.workspaces.map((w) => w.id)).toEqual(["w1", "c1", "w3"]); // not the bench, not another person's
  const [w1, c1, w3] = v.workspaces;
  expect(c1).toMatchObject({ parent: "w1", task: "probe" });
  expect(c1!.processes).toBeUndefined(); // pod answered 503
  expect(c1!.changes).toBeUndefined();
  expect(w3).toMatchObject({ attached_environment: "e1" });
  expect(w3!.processes).toBeUndefined(); // not ready: pod never asked
  expect(w1!.changes).toBe(2);
  expect(w1!.processes!.map((p) => p.id)).toEqual(["p1", "p2"]);
  expect(w1!.processes![0]!.logs).toHaveLength(20);
  expect(w1!.processes![0]!.logs.at(-1)).toBe("l29");
  expect(w1!.processes![1]!.logs).toEqual([]); // only running processes are read
  expect(v.environments[0]!.services).toEqual([{ name: "api", ports: [8080], interceptedBy: "w1" }, { name: "db", ports: [5432], interceptedBy: undefined }]);
  expect(JSON.stringify(v)).not.toContain("TOKEN");
  expect(readClones().map((r) => r.id)).toEqual(["c1"]); // the deleted one is pruned
  expect(urls.some((u) => u.includes("w3/tools"))).toBe(false);
});

test("a failing list call is unavailable, with the API's text and no token", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-space-"));
  writeFileSync(join(dir, "tok"), "SECRET-TOKEN\n");
  Object.assign(process.env, { KL_API_URL: "http://api", KL_TOOL_TOKEN_FILE: join(dir, "tok") });
  globalThis.fetch = (async () => new Response("forbidden", { status: 403 })) as any;
  const v = await new LocalBackend().space();
  expect(v.available).toBe(false);
  expect(v.error).toBe("error 403: forbidden");
});
