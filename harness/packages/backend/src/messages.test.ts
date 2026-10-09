import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { readMessages, recordMessage } from "./messages.ts";

const tmp = () => join(mkdtempSync(join(tmpdir(), "kl-msgs-")), "messages.json");

test("a missing log reads as []", () => expect(readMessages(tmp())).toEqual([]));

test("the log keeps the newest 100 and cuts text to 200 chars", () => {
  const f = tmp();
  for (let i = 0; i < 105; i++) recordMessage(f, { from: "main", to: "w", text: `${i}`.padEnd(300, "x") });
  const all = readMessages(f);
  expect(all.length).toBe(100);
  expect(all[0]!.text.startsWith("5x")).toBe(true);
  expect(all[99]!.text.length).toBe(200);
});
