import { expect, test } from "bun:test";
import { pct, capacityTone, sparkPath, initials } from "./console";

test("a percentage is bounded and a zero limit is not a division", () => {
  expect(pct(19, 20)).toBe(95);
  expect(pct(0, 0)).toBe(0);
  // Usage can exceed a limit that was lowered under a running fleet; the bar clamps rather
  // than overflowing its track.
  expect(pct(30, 20)).toBe(100);
});

test("the bar turns amber at 80% and red only at the wall", () => {
  expect(capacityTone(15, 20)).toBe("ok");
  expect(capacityTone(16, 20)).toBe("warn");
  expect(capacityTone(19, 20)).toBe("warn");
  expect(capacityTone(20, 20)).toBe("critical");
  // Over the limit is still the wall, not a fourth state.
  expect(capacityTone(21, 20)).toBe("critical");
});

test("a sparkline path spans the box and a flat series is a flat line", () => {
  const p = sparkPath([1, 5, 3], 100, 20);
  expect(p.startsWith("M0,")).toBe(true);
  expect(p).toContain("100,");
  // A constant series has no range to scale by; it must not produce NaN.
  expect(sparkPath([4, 4, 4], 100, 20)).toBe("M0,10 L50,10 L100,10");
  expect(sparkPath([], 100, 20)).toBe("");
});

test("initials come from the visible name, at most two letters", () => {
  expect(initials("karthik")).toBe("K");
  expect(initials("Amrita Mehta")).toBe("AM");
  expect(initials("karthik@kloudlite.io")).toBe("K");
  expect(initials("")).toBe("?");
});
