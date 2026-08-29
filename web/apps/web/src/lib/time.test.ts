import { describe, expect, test } from "bun:test";
import { size, stamp, when, whenSeconds } from "./time";

describe("when", () => {
  test("rounds to the coarsest unit that fits", () => {
    const now = Date.now();
    expect(when(now)).toBe("just now");
    expect(when(now - 30_000)).toBe("just now");
    expect(when(now - 5 * 60_000)).toBe("5 minutes ago");
    expect(when(now - 3 * 3_600_000)).toBe("3 hours ago");
    expect(when(now - 86_400_000)).toBe("yesterday");
    expect(when(now - 3 * 86_400_000)).toBe("3 days ago");
  });

  test("past a month it is a date", () => {
    expect(when(Date.UTC(2020, 0, 15))).toMatch(/Jan 1[45], 2020/);
    expect(whenSeconds(Date.UTC(2020, 0, 15) / 1000)).toMatch(/2020/);
  });
});

describe("stamp", () => {
  test("is the same string on every machine", () => {
    expect(stamp(Date.UTC(2024, 2, 5, 14, 7))).toBe("Mar 5, 2024, 2:07 PM UTC");
  });
});

describe("size", () => {
  test("picks the unit a person would", () => {
    expect(size(null)).toBe("");
    expect(size(0)).toBe("0 B");
    expect(size(1023)).toBe("1023 B");
    expect(size(1536)).toBe("2 KB");
    expect(size(3 * 1024 * 1024 + 200_000)).toBe("3.2 MB");
  });
});
