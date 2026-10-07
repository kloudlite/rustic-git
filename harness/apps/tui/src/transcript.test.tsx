import { expect, test } from "bun:test";
import { testRender } from "@opentui/react/test-utils";
import { Transcript, type Entry } from "./components/Transcript.tsx";

const entries: Entry[] = Array.from({ length: 60 }, (_, i) => ({
  kind: "user" as const,
  text: `line ${i}`,
}));

test("scrolling up offers a jump back to the bottom, end takes it", async () => {
  const t = await testRender(<Transcript entries={entries} />, {
    width: 80,
    height: 16,
    kittyKeyboard: true,
  });
  const frame = async () => {
    await new Promise((r) => setTimeout(r, 30));
    await t.renderOnce();
    return t.captureCharFrame();
  };
  await frame();
  expect(await frame()).not.toContain("jump to bottom"); // sticky at the bottom

  t.mockInput.pressKey("\x1b[5~"); // PageUp — mockInput has no constant for it
  expect(await frame()).toContain("jump to bottom");

  t.mockInput.pressKey("\x1b[F"); // End
  await new Promise((r) => setTimeout(r, 250)); // the at-bottom poll
  expect(await frame()).not.toContain("jump to bottom");
  t.renderer.destroy();
});

test("a long block collapses to its head and a click toggles it both ways", async () => {
  const long: Entry[] = [
    { id: "a", kind: "agent", text: Array.from({ length: 25 }, (_, i) => `line ${i + 1}`).join("\n") },
  ];
  const t = await testRender(<Transcript entries={long} keys="page" />, {
    width: 70,
    height: 24,
    kittyKeyboard: true,
  });
  const frame = async () => {
    await new Promise((r) => setTimeout(r, 30));
    await t.renderOnce();
    return t.captureCharFrame();
  };
  // the head is kept, not the tail — Claude Code reads from the beginning
  const first = await frame();
  expect(first).toContain("line 1");
  expect(first).not.toContain("line 25");
  expect(first).toContain("+15 lines");
  expect(first).toContain("to expand");

  // click only — opencode has no expand key, so neither do we
  await t.mockMouse.click(10, 3);
  const open = await frame();
  expect(open).toContain("line 25");
  expect(open).toContain("Click to collapse"); // the row stays, offering the way back

  await t.mockMouse.click(10, 3); // and it really collapses again
  expect(await frame()).not.toContain("line 25");
  t.renderer.destroy();
});

// reasoning is a ticker while it streams and a readable block once it lands —
// the old renderer clipped every thinking entry to one line, so a large
// thinking budget was invisible
test("thinking tickers while streaming and opens up when done", async () => {
  const long = Array.from({ length: 18 }, (_, i) => `reasoning line ${i + 1}`).join("\n");
  const t = await testRender(
    <Transcript
      entries={[
        { kind: "thinking", id: "a", text: "step one\nstep two\nstep three" },
        { kind: "thinking", id: "b", text: long, done: true },
      ]}
    />,
    { width: 90, height: 40 },
  );
  await new Promise((r) => setTimeout(r, 200));
  await t.renderOnce();
  const frame = t.captureCharFrame();
  // streaming: newest line only, earlier ones not kept
  expect(frame).toContain("step three");
  expect(frame).not.toContain("step one");
  // done: labelled, collapsed to its head, expandable
  expect(frame).toContain("Thinking");
  expect(frame).toContain("reasoning line 10");
  expect(frame).toContain("+8 lines");
  expect(frame).not.toContain("reasoning line 11");
}, 20000);

// Reaching the "… +N lines" row meant scrolling past the whole block first,
// so the block itself is the toggle — but a drag over it still selects text.
test("clicking a long block toggles it, dragging over it does not", async () => {
  const para =
    "Long unbroken reasoning paragraph that the terminal soft wraps across many visual rows and therefore collapses to a ten row head. ";
  const t = await testRender(
    <Transcript
      width={100}
      entries={[{ kind: "agent", id: "a1", text: Array(6).fill(para).join("\n") }]}
    />,
    { width: 100, height: 20 },
  );
  await new Promise((r) => setTimeout(r, 200));
  await t.renderOnce();
  expect(t.captureCharFrame()).toContain("Click to expand");


  await t.mockMouse.click(10, 5);
  await new Promise((r) => setTimeout(r, 80));
  await t.renderOnce();
  expect(t.captureCharFrame()).toContain("Click to collapse");

  // a drag is a selection, not a toggle
  await t.mockMouse.drag(10, 5, 60, 6);
  await new Promise((r) => setTimeout(r, 80));
  await t.renderOnce();
  expect(t.captureCharFrame()).toContain("Click to collapse");
}, 15000);

// codemode's argument is a whole script, not a one-line command, so it gets
// the bash block with its source where the command goes.
test("a codemode call renders its script above its output", async () => {
  const t = await testRender(
    <Transcript
      width={100}
      entries={[
        {
          kind: "tool",
          id: "c1",
          name: "codemode",
          status: "ok",
          summary: 'const f = await tools.read({path: "a.ts"});\nreturn f.length;',
          output: "Script completed\n42",
        },
      ]}
    />,
    { width: 100, height: 20 },
  );
  await new Promise((r) => setTimeout(r, 200));
  await t.renderOnce();
  const frame = t.captureCharFrame();
  expect(frame).toContain("codemode");
  expect(frame).toContain("tools.read");
  expect(frame).toContain("return f.length;");
  expect(frame).toContain("Script completed");
}, 15000);

// A bash command is text in the block and wraps like any other text, so it has
// to be counted: a `grep` over a lockfile is one very long line, and leaving it
// out of the row count sized the box short and let the output spill past it.
test("a long bash command counts toward the block's rows", async () => {
  // ~12 wrapped rows at width 98, with no output to speak of
  const cmd = `grep -n '"@opentui/react"' bun.lock | ${"echo some-fairly-long-argument ".repeat(40)}done`;
  const t = await testRender(
    <Transcript
      width={100}
      entries={[{ kind: "tool", id: "b1", name: "bash", status: "ok", summary: cmd, output: "ok" }]}
    />,
    { width: 100, height: 24 },
  );
  await new Promise((r) => setTimeout(r, 200));
  await t.renderOnce();
  expect(t.captureCharFrame()).toContain("to expand");
  t.renderer.destroy();
}, 15000);

// A reloaded session's transcript arrives from disk after the first paint, so
// an empty `entries` has two meanings — the welcome screen is only one of them.
test("the welcome screen waits for the transcript to be read", async () => {
  const t = await testRender(<Transcript entries={[]} ready={false} />, {
    width: 80,
    height: 20,
  });
  await new Promise((r) => setTimeout(r, 60));
  await t.renderOnce();
  expect(t.captureCharFrame()).not.toContain("Orchestrate agents");
  t.renderer.destroy();
}, 15000);

test("an empty session that has been read shows the welcome screen", async () => {
  const t = await testRender(<Transcript entries={[]} ready />, {
    width: 80,
    height: 20,
  });
  await new Promise((r) => setTimeout(r, 60));
  await t.renderOnce();
  expect(t.captureCharFrame()).toContain("Orchestrate agents");
  t.renderer.destroy();
}, 15000);
