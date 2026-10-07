import { test, expect } from "bun:test";
import { Registry, type ToolDef } from "@kloudlite-tui/tools";
import { adaptTools } from "@kloudlite-tui/agent";

// pi's execute() takes params as its SECOND argument (the first is the tool
// call id) and the call site casts through `as never`, so nothing at compile
// time catches a shift back to the first. This is the only guard: every custom
// tool silently loses its parameters if it regresses.
test("a custom tool receives its parameters, not its call id", async () => {
  let seen: unknown = "NEVER CALLED";
  const echo = {
    name: "echo_probe",
    description: "Echo the word back.",
    inputSchema: { type: "object", properties: { word: { type: "string" } }, required: ["word"] },
    async run(args) {
      seen = args;
      return `echoed ${JSON.stringify(args)}`;
    },
  } satisfies ToolDef<{ word: string }>;
  const [tool] = adaptTools(new Registry().add(echo));
  const out = await tool!.execute("call_abc123", { word: "banana" });
  expect(seen).toEqual({ word: "banana" });
  expect(out.content[0]!.text).toBe('echoed {"word":"banana"}');
});
