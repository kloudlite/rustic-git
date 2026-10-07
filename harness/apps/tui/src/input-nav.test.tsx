import { expect, test } from "bun:test";
import { testRender } from "@opentui/react/test-utils";
import { Input } from "./components/Input.tsx";

// ↑ inside a multiline value moves between lines; only the top line falls
// through to history, so a two-line prompt is editable without losing it.
test("up moves between lines and only recalls history at the top", async () => {
  const dirs: number[] = [];
  let value = "alpha\nbravo";
  const t = await testRender(
    <Input
      value={value}
      onChange={(v) => (value = v)}
      onSubmit={() => {}}
      placeholder=""
      showCursor
      onHistory={(d) => (dirs.push(d), true)}
    />,
    { width: 40, height: 6 },
  );
  const frame = async () => {
    await new Promise((r) => setTimeout(r, 30));
    await t.renderOnce();
    return t.captureCharFrame();
  };
  await frame();

  t.mockInput.pressKey("ARROW_UP"); // caret starts at the end → onto "alpha"
  await frame();
  expect(dirs).toEqual([]);
  t.mockInput.typeText("X");
  await frame();
  expect(value).toBe("alphaX\nbravo"); // moved up, not into history

  t.mockInput.pressKey("ARROW_UP"); // already on the first line
  await frame();
  expect(dirs).toEqual([-1]);
  t.renderer.destroy();
}, 15000);

// clicking puts the caret where the click landed, not at the end
test("a click moves the caret to that column", async () => {
  let value = "abcdef";
  const t = await testRender(
    <Input
      value={value}
      onChange={(v) => (value = v)}
      onSubmit={() => {}}
      placeholder=""
      showCursor
    />,
    { width: 40, height: 4 },
  );
  await new Promise((r) => setTimeout(r, 30));
  await t.renderOnce();
  await t.mockMouse.click(2, 0);
  await new Promise((r) => setTimeout(r, 30));
  await t.renderOnce();
  t.mockInput.typeText("X");
  await new Promise((r) => setTimeout(r, 30));
  await t.renderOnce();
  expect(value).toBe("abXcdef");
  t.renderer.destroy();
}, 15000);
