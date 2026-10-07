import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { readClipboardImage } from "./clipboard.ts";

// a 2x2 PNG, enough to put real image bytes on the clipboard
const PNG =
  "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAFElEQVR4nGP8z8DAwMDAxAADDAwMAA4OAQGHrPnJAAAAAElFTkSuQmCC";

const macos = process.platform === "darwin";

test.if(macos)("reads a PNG off the macOS clipboard, and nothing off a text one", async () => {
  const path = `${process.env.TMPDIR ?? "/tmp"}/kloudlite-clip-test.png`;
  await Bun.write(path, Buffer.from(PNG, "base64"));
  execFileSync("osascript", [
    "-e",
    `set the clipboard to (read (POSIX file "${path}") as «class PNGf»)`,
  ]);
  const img = readClipboardImage();
  expect(img?.mimeType).toBe("image/png");
  expect(img!.data.length).toBeGreaterThan(0);

  execFileSync("osascript", ["-e", 'set the clipboard to "just text"']);
  expect(readClipboardImage()).toBeNull(); // text must not read as an image
});

test("a clipboard with no image tool available is a no-op, never a throw", () => {
  expect(() => readClipboardImage()).not.toThrow();
});
