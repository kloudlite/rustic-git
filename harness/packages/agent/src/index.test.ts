import { expect, test } from "bun:test";
import { existsSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { declaredTools } from "./claude-tools.ts";

test("codemode only: pi declares codemode, not bash (the Claude bridge relies on it)", async () => {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  const { createSession, models } = await import("./index.ts");
  const model = models.getModels().find((m: any) => m.provider !== "anthropic") as any;
  const s: any = await createSession({ key: `t-${process.pid}`, model, codemode: true, fresh: true });
  const names = declaredTools(s).map((t: any) => t.name);
  expect(names).toContain("codemode");
  expect(names).not.toContain("bash");
  expect(s.agent.state.tools.map((t: any) => t.name)).toContain("bash"); // still callable from scripts
  expect(s.systemPrompt).toContain("codemode");
  s.dispose();
});

test("codemode description opens with the rules, display included", async () => {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  const { createSession, models } = await import("./index.ts");
  const model = models.getModels().find((m: any) => m.provider !== "anthropic") as any;
  const s: any = await createSession({ key: `t-${process.pid}-note`, model, codemode: true, fresh: true });
  const tool = declaredTools(s).find((t: any) => t.name === "codemode");
  expect(tool.description.startsWith("Rules: (1)")).toBe(true);
  expect(tool.description).toContain("tools.display({ markdown })");
  s.dispose();
});

test("codemode sessions advertise the codemode skill, from the plugin Claude sessions load", async () => {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  const { createSession, models } = await import("./index.ts");
  const { CODEMODE_SKILL } = await import("./claude.ts");
  expect(existsSync(join(CODEMODE_SKILL, "../../.claude-plugin/plugin.json"))).toBe(true);
  const model = models.getModels().find((m: any) => m.provider !== "anthropic") as any;
  const s: any = await createSession({ key: `t-${process.pid}-skill`, model, codemode: true, fresh: true });
  expect(s.systemPrompt).toContain("How to write codemode scripts");
  s.dispose();
});

function harness(execute: (id: string, params: any, signal: any, onUpdate: any) => Promise<any>) {
  const tools: Record<string, any> = {};
  const pi = { registerTool: (t: any) => (tools[t.name] = t) };
  const factory = (p: any) =>
    p.registerTool({ name: "codemode", prepareLoadout: () => ({ descriptions: { codemode: "orig" } }), execute });
  return { tools, run: async () => (await import("./index.ts")).withCodemodeExtras(factory as any)(pi as any) };
}

test("display pushes fold into the codemode result in order, and clear after", async () => {
  let h: ReturnType<typeof harness>;
  const updates: any[] = [];
  h = harness(async (id, _p, _s, onUpdate) => {
    await h.tools.display.execute(`${id}/1`, { markdown: "a" });
    onUpdate({ content: [], details: { calls: [] } });
    await h.tools.display.execute(`${id}/2`, { markdown: "b\nc" });
    return { content: [], details: { calls: [] } };
  });
  await h.run();
  expect(h.tools.display.exposure).toBe("codemode");
  expect(h.tools.codemode.prepareLoadout({}).descriptions.codemode).toStartWith("Rules:");
  const r = await h.tools.codemode.execute("x", {}, undefined, (u: any) => updates.push(u));
  expect(r.details.display).toEqual(["a", "b\nc"]);
  // the model reads only the codemode result, so the shown markdown must be in its content
  expect(r.content.at(-1).text).toEndWith("a\n\nb\nc");
  expect(updates[0].details.display).toEqual(["a"]);
  h = harness(async () => ({ content: [], details: {} }));
  await h.run();
  expect((await h.tools.codemode.execute("x", {}, undefined, undefined)).details.display).toBeUndefined();
});

test("display: throw path clears, empty and over-cap are refused", async () => {
  const h = harness(async (id) => {
    await h.tools.display.execute(`${id}/1`, { markdown: "a" });
    throw new Error("boom");
  });
  await h.run();
  await expect(h.tools.codemode.execute("y", {}, undefined, undefined)).rejects.toThrow("boom");
  const lone = await h.tools.display.execute("y/1", { markdown: "z" }); // would leak into "y" if not cleared
  await expect(h.tools.display.execute("y/2", { markdown: "  " })).rejects.toThrow("markdown is empty");
  const big = await h.tools.display.execute("y/3", { markdown: "q".repeat(200_001) });
  expect(big.content[0].text).toBe("display: limit reached, not shown");
  expect(lone.content[0].text).toContain("Shown to the user (1 lines)");
});
