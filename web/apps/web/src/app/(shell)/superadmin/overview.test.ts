import { describe, expect, test } from "bun:test";
import { needsNothing, regionCapacity } from "./overview";
import { FLAT } from "@/lib/history";

const s = (last: number) => ({ series: [{ ts: "t", value: last }], summary: { last, delta: 0, min: last, max: last }, available: true });

test("a region's three gauges are the latest sample of each node series", () => {
  const c = regionCapacity(s(0.8), s(0.72), s(0.76));
  expect(c.pool).toEqual({ used: 80, limit: 100, unit: "%" });
  expect(c.cpu.used).toBe(72);
  expect(c.memory.used).toBe(76);
});

test("a region with no history shows an empty gauge, not a full one", () => {
  const c = regionCapacity(FLAT, FLAT, FLAT);
  expect(c.pool).toEqual({ used: 0, limit: 0, unit: "%" });
});

describe("needsNothing", () => {
  test("true when both are empty", () => {
    expect(needsNothing({ pendingRequests: [], attention: [] })).toBe(true);
  });

  test("false with a pending request", () => {
    expect(needsNothing({ pendingRequests: [{} as never], attention: [] })).toBe(false);
  });

  test("false with an attention item", () => {
    expect(needsNothing({ pendingRequests: [], attention: [{} as never] })).toBe(false);
  });
});
