// The laptop binary must not carry the agent: remote.tsx's bundle is the proof.
import { expect, test } from "bun:test";
import { join } from "node:path";

test("remote.tsx bundles no agent", async () => {
  const out = await Bun.build({ entrypoints: [join(import.meta.dir, "remote.tsx")], target: "bun", external: ["@opentui/*"] });
  expect(out.success).toBe(true);
  const code = (await Promise.all(out.outputs.map((o) => o.text()))).join("");
  expect(code).not.toContain("ModelRuntime");
  expect(code).not.toContain("claude-agent-sdk");
});

test("TUI keeps the backend's gate sets", async () => {
  const { ALWAYS_ASK, ASK_UNLESS_FENCED, EDITS } = await import("@kloudlite-tui/backend/local");
  const app = await Bun.file(join(import.meta.dir, "app.tsx")).text();
  for (const t of [...ALWAYS_ASK, ...ASK_UNLESS_FENCED]) expect(app).toContain(`"${t}"`);
  expect([...EDITS]).toEqual(["write", "edit", "patch"]);
});
