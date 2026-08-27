import { expect, test } from "bun:test";
import { mergeFeeds } from "./home";

test("feeds merge newest first and cap", () => {
  const a = [{ at: 30 }, { at: 10 }] as never[];
  const b = [{ at: 20 }] as never[];
  expect(mergeFeeds([a, b], 2).map((e: { at: number }) => e.at)).toEqual([30, 20]);
});
