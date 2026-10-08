import { afterEach, beforeEach, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { api, platformTools } from "./platform.ts";

type Call = { method: string; url: string; auth: string | null; body: any };
let calls: Call[] = [];
let routes: Record<string, () => Response> = {};
const realFetch = globalThis.fetch;
let tokenFile = "";

beforeEach(() => {
  calls = [];
  routes = {};
  tokenFile = join(mkdtempSync(join(tmpdir(), "kl-tok-")), "token");
  writeFileSync(tokenFile, "tok1\n");
  process.env.KL_API_URL = "http://api.test";
  process.env.KL_TOOL_TOKEN_FILE = tokenFile;
  globalThis.fetch = (async (url: string, init: any = {}) => {
    const u = String(url).replace("http://api.test", "");
    calls.push({ method: init.method ?? "GET", url: u, auth: init.headers?.authorization ?? null, body: init.body ? JSON.parse(init.body) : undefined });
    const r = routes[`${init.method ?? "GET"} ${u}`];
    return r ? r() : new Response("not found", { status: 404 });
  }) as any;
});
afterEach(() => {
  globalThis.fetch = realFetch;
  delete process.env.KL_API_URL;
  delete process.env.KL_TOOL_TOKEN_FILE;
});

const json = (v: unknown, status = 200) => () => new Response(JSON.stringify(v), { status });
const tool = (tools: ReturnType<typeof platformTools>, n: string) => tools.find((t) => t.name === n)!;

test("api reads the token file on every call, reports unavailable, 204 and errors", async () => {
  routes["GET /x"] = json({ a: 1 });
  routes["DELETE /x"] = () => new Response(null, { status: 204 });
  routes["POST /x"] = () => new Response('{"error":"busy"}', { status: 409 });
  expect(await api("GET", "/x")).toBe('{\n  "a": 1\n}');
  writeFileSync(tokenFile, "tok2");
  await api("GET", "/x");
  expect(calls.map((c) => c.auth)).toEqual(["Bearer tok1", "Bearer tok2"]);
  expect(await api("DELETE", "/x")).toBe("ok");
  await expect(api("POST", "/x", {})).rejects.toThrow('409: {"error":"busy"}');
  delete process.env.KL_API_URL;
  await expect(api("GET", "/x")).rejects.toThrow("platform tools unavailable");
});

test("packages_add merges by attr, packages_remove errors on unknown", async () => {
  const t = platformTools("main");
  routes["GET /v1/workspaces/w1"] = json({ packages: ["nodejs", "git@2"] });
  routes["PATCH /v1/workspaces/w1"] = json({});
  await tool(t, "packages_add").run({ workspace: "w1", packages: ["nodejs@20", "jq"] });
  expect(calls.at(-1)!.body).toEqual({ packages: ["git@2", "nodejs@20", "jq"] });
  await tool(t, "packages_remove").run({ workspace: "w1", packages: ["git"] });
  expect(calls.at(-1)!.body).toEqual({ packages: ["nodejs"] });
  const n = calls.length;
  expect(await tool(t, "packages_remove").run({ workspace: "w1", packages: ["zzz"] })).toBe("error: not declared: zzz");
  expect(calls.length).toBe(n + 1); // the GET only, no PATCH
});

test("service add/update/remove PATCH the whole list; duplicates and strangers error", async () => {
  const t = platformTools("main");
  const a = { name: "a", image: "a:1" };
  const b = { name: "b", image: "b:1" };
  routes["GET /v1/environments/e1"] = json({ services: [a] });
  routes["PATCH /v1/environments/e1"] = json({});
  await tool(t, "service_add").run({ env: "e1", service: b });
  expect(calls.at(-1)!.body).toEqual({ services: [a, { ...b, command: [], env: {}, mounts: [] }] });
  await tool(t, "service_update").run({ env: "e1", service: { name: "a", image: "a:2" } });
  expect(calls.at(-1)!.body).toEqual({ services: [{ name: "a", image: "a:2", command: [], env: {}, mounts: [] }] });
  await tool(t, "service_update").run({ env: "e1", service: { name: "a", image: "a:3", env: { K: "v" } } });
  expect(calls.at(-1)!.body).toEqual({ services: [{ name: "a", image: "a:3", command: [], env: { K: "v" }, mounts: [] }] });
  await tool(t, "service_remove").run({ env: "e1", name: "a" });
  expect(calls.at(-1)!.body).toEqual({ services: [] });
  expect(await tool(t, "service_add").run({ env: "e1", service: a })).toContain("error: service exists");
  expect(await tool(t, "service_update").run({ env: "e1", service: b })).toContain("error: no such service");
  expect(await tool(t, "service_remove").run({ env: "e1", name: "q" })).toContain("error: no such service");
});

test("workspace sessions default env through /v1/me/environments", async () => {
  const t = platformTools("workspace", "w1");
  routes["GET /v1/workspaces/w1"] = json({ team: "acme", packages: [] });
  routes["GET /v1/me/environments"] = json([{ team: "other", environment: "x" }, { team: "acme", environment: "e9" }]);
  routes["GET /v1/environments/e9"] = json({ services: [] });
  routes["POST /v1/environments/e9/intercepts"] = json({}, 202);
  await tool(t, "env_get").run({});
  expect(calls.at(-1)!.url).toBe("/v1/environments/e9");
  await tool(t, "intercept").run({ service: "web" });
  expect(calls.at(-1)!.body).toEqual({ service: "web", workspace: "w1" });
  routes["GET /v1/me/environments"] = json([{ team: "other", environment: "x" }]);
  expect(await tool(t, "env_get").run({})).toBe("error: this workspace's space follows no environment");
  expect(t.some((x) => x.name === "workspace_create" || x.name === "workspace_stop")).toBe(false);
});

test("env_get folds service_status readiness into services[]; workspace_clone sends task only when given", async () => {
  const t = platformTools("main");
  routes["GET /v1/environments/e1"] = json({ services: [{ name: "a" }, { name: "b" }], service_status: [{ name: "a", ready: true }, { name: "b", ready: false, message: "pulling" }] });
  const out = JSON.parse(await tool(t, "env_get").run({ env: "e1" }));
  expect(out.services).toEqual([{ name: "a", ready: true }, { name: "b", ready: false, message: "pulling" }]);
  expect(out.service_status.length).toBe(2);
  routes["POST /v1/workspaces/w1/clone"] = json({}, 202);
  await tool(t, "workspace_clone").run({ workspace: "w1", name: "c" });
  expect(calls.at(-1)!.body).toEqual({ name: "c" });
  await tool(t, "workspace_clone").run({ workspace: "w1", name: "c", task: "try x" });
  expect(calls.at(-1)!.body).toEqual({ name: "c", task: "try x" });
});

test("a rejected service_add throws and sends no further call", async () => {
  const t = platformTools("main");
  routes["GET /v1/environments/e1"] = json({ services: [] });
  routes["PATCH /v1/environments/e1"] = json({ error: "services[0]: missing field `command`" }, 422);
  await expect(tool(t, "service_add").run({ env: "e1", service: { name: "n", image: "n:1" } })).rejects.toThrow("422:");
});
