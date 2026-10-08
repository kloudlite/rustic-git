import { expect, test } from "bun:test";
import { uptime } from "./Processes.tsx";

test("uptime: <1m, minutes, hours and minutes, days and hours", () => {
  const t0 = Date.parse("2026-10-09T00:00:00Z");
  const at = (ms: number) => uptime("2026-10-09T00:00:00Z", t0 + ms);
  expect(at(30_000)).toBe("<1m");
  expect(at(4 * 60_000)).toBe("4m");
  expect(at(72 * 60_000)).toBe("1h 12m");
  expect(at(51 * 3_600_000)).toBe("2d 3h");
  expect(uptime(undefined, t0)).toBe("");
});
