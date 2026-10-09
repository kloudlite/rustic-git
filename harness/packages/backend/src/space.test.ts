import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
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

test("lists the user's workspaces and the team's environments, reads clone parent and task from the doc, survives a dead pod", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-space-"));
  writeFileSync(join(dir, "tok"), "SECRET-TOKEN\n");
  Object.assign(process.env, { KL_API_URL: "http://api", KL_TOOL_TOKEN_FILE: join(dir, "tok"), KL_OWNER: "me", KL_TEAM: "acme", KL_BENCH: "bench" });
  const urls: string[] = [];
  const json = (v: unknown, status = 200) => new Response(JSON.stringify(v), { status });
  globalThis.fetch = (async (u: any, init: any) => {
    const url = String(u);
    urls.push(`${init?.method ?? "GET"} ${url}`);
    expect(init.headers.authorization).toBe(url.startsWith("http://api") ? "Bearer SECRET-TOKEN" : "Bearer POD-TOKEN");
    if (url === "http://api/v1/workspaces?team=acme")
      return json([
        { id: "w1", name: "api", owner: "me", state: "ready", repo: "o/r", branch: "main" },
        { id: "c1", name: "sub", owner: "me", state: "ready", clone_of: "w1", task: "probe" },
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
  expect(w1!.processes![0]!.logs).toHaveLength(30);
  expect(w1!.processes![0]!.logs.at(-1)).toEqual({ text: "l29" });
  expect(w1!.processes![1]!.logs).toHaveLength(30); // an exited process is read too, once
  expect(v.environments[0]!.services).toEqual([{ name: "api", ports: [8080], interceptedBy: "w1" }, { name: "db", ports: [5432], interceptedBy: undefined }]);
  expect(JSON.stringify(v)).not.toContain("TOKEN");
  expect(urls.some((u) => u.includes("w3/tools"))).toBe(false);
});

test("a running process is read from the cursor the last answer gave, not from 0", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-space-"));
  writeFileSync(join(dir, "tok"), "SECRET-TOKEN\n");
  Object.assign(process.env, { KL_API_URL: "http://api", KL_TOOL_TOKEN_FILE: join(dir, "tok"), KL_OWNER: "me", KL_TEAM: "acme", KL_BENCH: "bench" });
  const json = (v: unknown) => new Response(JSON.stringify(v));
  const reads: any[] = [];
  globalThis.fetch = (async (u: any, init: any) => {
    const url = String(u);
    if (url.startsWith("http://api/v1/workspaces?")) return json([{ id: "wc", name: "cur", owner: "me", state: "ready" }]);
    if (url.startsWith("http://api/v1/environments")) return json([]);
    if (url.endsWith("/v1/me/environments")) return json([]);
    if (url.endsWith("/v1/workspaces/wc/tools")) return json({ address: "10.0.0.2:7788", token: "T" });
    if (url.endsWith("/tools/process_list")) return json({ processes: [{ id: "px", cmd: "srv", state: "running", exit_code: null, failed: false }] });
    if (url.endsWith("/tools/process_output")) {
      const body = JSON.parse(init.body);
      reads.push(body);
      return reads.length === 1 ? json({ stdout: "a\nb\n", stderr: "e1\n", next: 4, next_err: 3 }) : json({ stdout: "c\n", stderr: "", next: 6, next_err: 3 });
    }
    return json({ error: "nope" });
  }) as any;
  const b = new LocalBackend();
  const first = await b.space();
  expect(first.workspaces[0]!.processes![0]!.logs).toEqual([{ text: "a" }, { text: "b" }, { text: "e1", err: true }]);
  const second = await b.space();
  expect(reads[1]).toMatchObject({ id: "px", since: 4, since_err: 3 });
  expect(second.workspaces[0]!.processes![0]!.logs).toEqual([{ text: "a" }, { text: "b" }, { text: "e1", err: true }, { text: "c" }]);
});

test("an exited process never seen running is read once; stderr lines are tagged", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-space-"));
  writeFileSync(join(dir, "tok"), "SECRET-TOKEN\n");
  Object.assign(process.env, { KL_API_URL: "http://api", KL_TOOL_TOKEN_FILE: join(dir, "tok"), KL_OWNER: "me", KL_TEAM: "acme", KL_BENCH: "bench" });
  const json = (v: unknown) => new Response(JSON.stringify(v));
  let reads = 0;
  globalThis.fetch = (async (u: any) => {
    const url = String(u);
    if (url.startsWith("http://api/v1/workspaces?")) return json([{ id: "wd", name: "dead", owner: "me", state: "ready" }]);
    if (url.startsWith("http://api/v1/environments")) return json([]);
    if (url.endsWith("/v1/me/environments")) return json([]);
    if (url.endsWith("/v1/workspaces/wd/tools")) return json({ address: "10.0.0.3:7788", token: "T" });
    if (url.endsWith("/tools/process_list")) return json({ processes: [{ id: "pd", cmd: "boom", state: "exited", exit_code: 1, failed: true, started_at: "2026-10-01T00:00:00Z" }] });
    if (url.endsWith("/tools/process_output")) {
      reads++;
      return json({ stdout: "out\n", stderr: "bad\n", next: 4, next_err: 4 });
    }
    return json({ error: "nope" });
  }) as any;
  const b = new LocalBackend();
  const first = await b.space();
  const p = first.workspaces[0]!.processes![0]!;
  expect(p.started_at).toBe("2026-10-01T00:00:00Z");
  expect(p.logs).toEqual([{ text: "out" }, { text: "bad", err: true }]);
  const second = await b.space();
  expect(reads).toBe(1);
  expect(second.workspaces[0]!.processes![0]!.logs).toEqual(p.logs);
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

test("a worktree's process is listed with its tree, and its logs are read from that tree", async () => {
  const dir = mkdtempSync(join(tmpdir(), "kl-space-"));
  writeFileSync(join(dir, "tok"), "SECRET-TOKEN\n");
  Object.assign(process.env, { KL_API_URL: "http://api", KL_TOOL_TOKEN_FILE: join(dir, "tok"), KL_OWNER: "me", KL_TEAM: "acme", KL_BENCH: "bench" });
  const json = (v: unknown) => new Response(JSON.stringify(v));
  const outputs: any[] = [];
  globalThis.fetch = (async (u: any, init: any) => {
    const url = String(u);
    if (url.startsWith("http://api/v1/workspaces?")) return json([{ id: "wc", name: "cur", owner: "me", state: "ready" }]);
    if (url.startsWith("http://api/v1/environments")) return json([]);
    if (url.endsWith("/v1/me/environments")) return json([]);
    if (url.endsWith("/v1/workspaces/wc/tools")) return json({ address: "10.0.0.2:7788", token: "T" });
    if (url.endsWith("/healthz")) return json({ graphs: { main: "ready", x: "ready" } });
    if (url.endsWith("/tools/process_list")) {
      const body = JSON.parse(init.body);
      return json({ processes: body.tree === "x" ? [{ id: "px", cmd: "go run .", state: "running", exit_code: null, failed: false }] : [] });
    }
    if (url.endsWith("/tools/process_output")) {
      outputs.push(JSON.parse(init.body));
      return json({ stdout: "up\n", stderr: "", next: 3, next_err: 0 });
    }
    return json({ error: "nope" });
  }) as any;
  const s = await new LocalBackend().space();
  const p = s.workspaces[0]!.processes![0]!;
  expect(p.tree).toBe("x");
  expect(outputs[0]).toMatchObject({ id: "px", tree: "x" });
});
