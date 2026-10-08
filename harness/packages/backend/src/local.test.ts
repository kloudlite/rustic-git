import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { LocalBackend, GATED, EDITS, installGate, registryFor, sessionKind } from "./local.ts";
import { toolDiff } from "./diff.ts";
import { PROTOCOL } from "./wire.ts";

test("hello carries what the TUI reads at boot", async () => {
  const h = await new LocalBackend().hello();
  expect(h.protocol).toBe(PROTOCOL);
  expect(h.cwd).toBe(process.cwd());
  expect(h.tools).toContain("workspace_create");
  expect(h.tools).toContain("workspace_ask");
  expect(h.tools).toContain("bash");
  expect(h.tools.slice(0, 2)).toEqual(["web_fetch", "web_search"]);
  expect(h.catalog.length).toBeGreaterThan(0);
  expect(h.catalog[0]).toHaveProperty("input");
  expect(h.defaultModel).toHaveProperty("provider");
});

test("settings write lands in the next hello", async () => {
  const b = new LocalBackend();
  await b.settings.write({ theme: "kloudlite-light" });
  expect((await b.hello()).settings.theme).toBe("kloudlite-light");
});

test("fs wraps git.ts", async () => {
  const b = new LocalBackend();
  expect(typeof (await b.fs.isGitRepo(process.cwd()))).toBe("boolean");
  expect(Array.isArray(await b.fs.listDir(process.cwd(), ""))).toBe(true);
});

test("gate set covers code writes and destructive platform verbs", () => {
  for (const n of ["bash", "write", "edit", "patch", "exec", "web_fetch", "workspace_stop", "workspace_delete", "worktree_drop", "env_delete", "env_stop", "env_restore_in_place", "service_remove", "volume_delete", "snapshot_delete"])
    expect(GATED.has(n)).toBe(true);
  expect(GATED.size).toBe(15);
  expect([...EDITS].sort()).toEqual(["edit", "patch", "write"]);
});

test("pod edit ({old,new}) has no local diff; pi edit still does", () => {
  expect(toolDiff("edit", { path: "a", edits: [{ old: "x", new: "y" }] })).toBeNull();
});

test("sessionKind", () => {
  expect(sessionKind("main")).toEqual({ kind: "main" });
  expect(sessionKind("main:x")).toEqual({ kind: "main" });
  expect(sessionKind("ws1")).toEqual({ kind: "workspace", ws: "ws1" });
  expect(sessionKind("ws1:x")).toEqual({ kind: "workspace", ws: "ws1" });
  expect(sessionKind("ws1:agent-ab12")).toEqual({ kind: "subagent", ws: "ws1" });
});

test("registry per session kind", async () => {
  delete process.env.KL_API_URL; // pod tools fall back to the fixed names
  const deps = { live: new Map(), busy: new Set<string>(), open: async () => null as never };
  const opts: any = { tools: [{ name: "question", description: "", inputSchema: {}, run: async () => "" }] };
  const main = (await registryFor({ kind: "main" }, deps, opts)).names();
  expect(main).toContain("workspace_create");
  expect(main).toContain("workspace_ask");
  expect(main).toContain("question");
  for (const n of ["bash", "read", "write"]) expect(main).toContain(n);
  expect(main).not.toContain("exec");
  const wsReg = await registryFor({ kind: "workspace", ws: "w1" }, deps, opts);
  const ws = wsReg.names();
  expect(ws).toContain("exec");
  expect(ws).toContain("packages_add");
  expect(ws).not.toContain("workspace_create");
  expect(ws).not.toContain("workspace_ask");
  expect(wsReg.get("read").description).not.toContain("scratch");
  expect(ws).toContain("subagent");
  expect((wsReg.get("packages_add").inputSchema as any).properties.workspace).toBeUndefined();
  const sub = (await registryFor({ kind: "subagent", ws: "w1" }, deps, opts)).names();
  expect(sub).not.toContain("question");
  expect(sub.filter((n) => n !== "web_fetch")).not.toContain("packages_add");
  expect(sub).toContain("web_fetch");
  expect(sub).toContain("exec");
  expect(sub).not.toContain("web_search");
  expect(sub).not.toContain("subagent");
});

test(":agent- sessions are hidden from sessions.list", async () => {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  delete process.env.KL_API_URL;
  const { models } = await import("@kloudlite-tui/agent");
  const m = models.getModels().find((x: any) => x.provider !== "anthropic") as any;
  const b = new LocalBackend();
  const o: any = { model: { provider: m.provider, id: m.id }, fresh: true, tools: [], permission: async () => ({}) };
  const key = `w9:agent-ab12`;
  const h = await b.session(key, o);
  const h2 = await b.session("w9", o);
  const keys = (await b.sessions.list()).map((s) => s.key);
  expect(keys).toContain("w9");
  expect(keys).not.toContain(key);
  await h.dispose();
  await h2.dispose();
}, 20000);

test("the gate asks for a codemode script's nested gated call, once for a top-level one", async () => {
  const seen: string[] = [];
  const pi: any = {
    agent: {} as any,
    async _beforeToolCall(_ctx: any, _parent?: string) {
      seen.push("pi");
      return undefined;
    },
  };
  pi.agent.beforeToolCall = (ctx: any) => pi._beforeToolCall(ctx);
  const asked: string[] = [];
  installGate(pi, async (req) => {
    asked.push(req.name);
    return req.name === "exec" ? { block: true, reason: "no" } : {};
  });
  const call = (name: string) => ({ toolCall: { name }, args: {} });
  // nested: pi's runner calls _beforeToolCall with the parent id, never agent.beforeToolCall
  expect(await pi._beforeToolCall(call("exec"), "cm1")).toEqual({ block: true, reason: "no" });
  expect(await pi._beforeToolCall(call("read"), "cm1")).toBeUndefined();
  // top level: asked once, then falls through to pi's own hook
  await pi.agent.beforeToolCall(call("bash"));
  expect(asked).toEqual(["exec", "bash"]);
  expect(seen).toEqual(["pi", "pi"]);
});
