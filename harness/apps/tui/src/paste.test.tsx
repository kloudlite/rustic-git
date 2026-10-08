import { afterAll, expect, test, mock } from "bun:test";
import * as clipboard from "./clipboard.ts";

// restore afterwards: mock.module is process-wide and clipboard.test.ts
// exercises the real reader
const real = clipboard.readClipboardImage;
mock.module("./clipboard.ts", () => ({
  ...clipboard,
  readClipboardImage: () => ({ type: "image", data: "x", mimeType: "image/png" }),
}));
afterAll(() => {
  mock.module("./clipboard.ts", () => ({ ...clipboard, readClipboardImage: real }));
});
import { testRender } from "@opentui/react/test-utils";
import { App } from "./app.tsx";
import { backend, boot, hello } from "./hello.ts";

// App reads its settings from hello() once at mount, so tests seed them by re-booting.
function writeSettings(patch: Record<string, unknown>) {
  boot(backend(), { ...hello(), settings: { ...hello().settings, ...patch } });
}

// a pasted image is a token in the value, so typing continues after it
test("pasted images keep their place in the prompt", async () => {
  writeSettings({ vim: "off", sidebarWidth: 42 });
  const t = await testRender(<App />, {
    width: 160,
    height: 34,
    kittyKeyboard: true,
  });
  await new Promise((r) => setTimeout(r, 400));
  await t.renderOnce();
  t.mockInput.pressKey("v", { ctrl: true });
  await t.renderOnce();
  t.mockInput.typeText("hello ");
  await t.renderOnce();
  t.mockInput.pressKey("v", { ctrl: true });
  await t.renderOnce();
  t.mockInput.typeText("world");
  await new Promise((r) => setTimeout(r, 100));
  await t.renderOnce();
  expect(t.captureCharFrame()).toContain("[Image 1] hello [Image 2] world");
});

// the badge is one thing on screen, so backspace takes all of it
test("backspace deletes a whole image token", async () => {
  writeSettings({ vim: "off", sidebarWidth: 42 });
  const t = await testRender(<App />, {
    width: 160,
    height: 34,
    kittyKeyboard: true,
  });
  const tick = async () => {
    await new Promise((r) => setTimeout(r, 60));
    await t.renderOnce();
  };
  await new Promise((r) => setTimeout(r, 400));
  await t.renderOnce();
  t.mockInput.pressKey("v", { ctrl: true });
  await tick();
  t.mockInput.typeText("hi ");
  await tick();
  t.mockInput.pressKey("v", { ctrl: true });
  await tick();
  expect(t.captureCharFrame()).toContain("[Image 1] hi [Image 2]");

  t.mockInput.pressKey("BACKSPACE");
  await tick();
  const frame = t.captureCharFrame();
  expect(frame).toContain("[Image 1] hi");
  expect(frame).not.toContain("[Image 2");
});
