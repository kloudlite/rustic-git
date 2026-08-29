import { describe, expect, test } from "bun:test";
import { commitBody, commitTitle, dayBucket } from "./commit-meta";

describe("commit message", () => {
  test("title is the first line, body follows the blank", () => {
    expect(commitTitle("Fix it\n\nBecause.")).toBe("Fix it");
    expect(commitBody("Fix it\n\nBecause.")).toBe("Because.");
    expect(commitBody("Fix it")).toBeUndefined();
  });
});

describe("dayBucket", () => {
  const noon = Date.UTC(2024, 4, 10, 12);
  test("calendar days in UTC, so every replica agrees", () => {
    expect(dayBucket(noon / 1000 - 3600, noon)).toBe("Today");
    // 1am UTC yesterday is still yesterday, however few hours ago it was.
    expect(dayBucket(Date.UTC(2024, 4, 9, 23) / 1000, Date.UTC(2024, 4, 10, 1))).toBe("Yesterday");
    expect(dayBucket(Date.UTC(2024, 4, 1) / 1000, noon)).toBe("May 1, 2024");
  });
});
