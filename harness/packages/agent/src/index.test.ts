import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
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
