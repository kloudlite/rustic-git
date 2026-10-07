import { test, expect } from "bun:test";
import { testRender } from "@opentui/react/test-utils";
import { Transcript } from "./components/Transcript";
import type { Entry } from "./components/Transcript";

// The hand-rolled renderer this replaces understood only **bold** and `code`,
// so a table arrived as raw pipes. Collapsing also used to rejoin soft-wrapped
// fragments, which split one table row across several and broke the parse.
test("an agent message renders markdown tables as real tables", async () => {
  const entries: Entry[] = [
    {
      kind: "agent",
      id: "a",
      text: "intro\n\n| Feature | A | B |\n| --- | --- | --- |\n| collapse | never | 10 rows |\n",
    },
  ];
  const t = await testRender(<Transcript entries={entries} width={76} />, {
    width: 80,
    height: 20,
  });
  await new Promise((r) => setTimeout(r, 400));
  await t.renderOnce();
  const frame = t.captureCharFrame();
  expect(frame).toContain("┌");
  expect(frame).toContain("│");
  expect(frame).not.toContain("| --- |");
});

test("markdown lists and fenced code lose their source markers", async () => {
  const entries: Entry[] = [
    { kind: "agent", id: "a", text: "# Title\n\n1. first\n2. second\n\n```ts\nconst x = 1;\n```\n" },
  ];
  const t = await testRender(<Transcript entries={entries} width={76} />, {
    width: 80,
    height: 20,
  });
  await new Promise((r) => setTimeout(r, 400));
  await t.renderOnce();
  const frame = t.captureCharFrame();
  expect(frame).toContain("Title");
  expect(frame).not.toContain("# Title");
  expect(frame).not.toContain("```");
  expect(frame).toContain("const x = 1;");
});
