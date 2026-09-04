import { describe, expect, test } from "bun:test";
import { filterAttention, regionCapacity } from "./overview";
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

describe("filterAttention", () => {
  const items = [
    { kind: "signal.firing" },
    { kind: "over_quota" },
    { kind: "draining" },
  ];

  test("all keeps every row", () => {
    expect(filterAttention(items, "all")).toHaveLength(3);
  });

  test("critical keeps only the rows attentionTone calls critical", () => {
    expect(filterAttention(items, "critical")).toEqual([{ kind: "signal.firing" }]);
  });

  test("warning keeps an unenumerated kind, because unknown kinds are warn", () => {
    // The guarded failure: filtering on the kind string instead of the tone would drop
    // `over_quota` from every tab the moment nobody had listed it.
    expect(filterAttention(items, "warn")).toEqual([{ kind: "over_quota" }]);
  });
});
