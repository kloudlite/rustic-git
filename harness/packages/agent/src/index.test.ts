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

test("codemode description tells the model tools.bash resolves to an object", async () => {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  const { createSession, models } = await import("./index.ts");
  const model = models.getModels().find((m: any) => m.provider !== "anthropic") as any;
  const s: any = await createSession({ key: `t-${process.pid}-note`, model, codemode: true, fresh: true });
  const tool = declaredTools(s).find((t: any) => t.name === "codemode");
  expect(tool.description).toContain("resolves to an object, not a string");
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
