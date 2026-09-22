import { test } from "node:test";
import assert from "node:assert/strict";

test("the engine loads without pi and exposes the seam", async () => {
  const e = await import("../src/engine/index.ts");
  for (const k of ["runTask", "ask", "makeAiSdkLlm", "TOOLS", "runTool", "readNotes", "addNotes"]) assert.ok(k in e, k);
  assert.ok(e.TOOLS.some((t: { name: string }) => t.name === "bash"));
});
