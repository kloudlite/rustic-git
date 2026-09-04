import { describe, expect, test } from "bun:test";
import { atLimit, dimFromRefusal, dimLabel, percent, requestedDiffs, tightestRatio, type QuotaReport } from "@/lib/quota";

const report = (used: number, limit: number): QuotaReport => ({
  owner: "karthik",
  limit: { workspaces: limit, environments: 2, snapshots: 20, diskGb: 100, cpu: 8, memoryGb: 32 },
  used: { workspaces: used, environments: 0, snapshots: 0, diskGb: 0, cpu: 0, memoryGb: 0 },
});

describe("percent", () => {
  test("is a whole percentage of the limit", () => {
    expect(percent(1, 4)).toBe(25);
    expect(percent(5, 5)).toBe(100);
  });
  // A limit of zero is a dimension nobody may use; the bar must read full, not divide by zero.
  test("a zero limit is full, never NaN", () => {
    expect(percent(0, 0)).toBe(100);
  });
  // Over-quota is possible: /v1 is read-then-write, and a limit can be lowered under existing use.
  test("clamps above the limit rather than overflowing the track", () => {
    expect(percent(7, 5)).toBe(100);
  });
});

test("atLimit is true only when there is no room left", () => {
  expect(atLimit(report(4, 5), "workspaces")).toBe(false);
  expect(atLimit(report(5, 5), "workspaces")).toBe(true);
  expect(atLimit(report(6, 5), "workspaces")).toBe(true);
});

// The 409's sentence is the contract between the api and this form: the dimension it names is the
// field the dialog pre-fills.
test("the refusal sentence names the dimension to ask about", () => {
  expect(dimFromRefusal("workspaces: 5 of 5 in use; request more under Quota")).toBe("workspaces");
  expect(dimFromRefusal("diskGb: 96 of 100 in use; request more under Quota")).toBe("diskGb");
  expect(dimFromRefusal("a workspace named \"x\" already exists here")).toBe(null);
});

test("every dimension has a label", () => {
  expect(dimLabel("diskGb")).toBe("Disk");
  expect(dimLabel("memoryGb")).toBe("Memory");
});

describe("tightestRatio", () => {
  const limit = { workspaces: 20, environments: 8, snapshots: 80, diskGb: 400, cpu: 32, memoryGb: 128 };
  test("is the smallest headroom ratio across all six dimensions", () => {
    const used = { workspaces: 19, environments: 6, snapshots: 64, diskGb: 310, cpu: 22, memoryGb: 96 };
    // workspaces: (20-19)/20 = 0.05, the tightest of the six.
    expect(tightestRatio(limit, used)).toBeCloseTo(0.05);
  });
  test("a zero limit is the tightest possible, ahead of a merely full one", () => {
    const used = { workspaces: 20, environments: 0, snapshots: 0, diskGb: 0, cpu: 0, memoryGb: 0 };
    expect(tightestRatio({ ...limit, environments: 0 }, used)).toBe(-Infinity);
  });
});

test("requestedDiffs only lists dimensions the request touched", () => {
  const limit = { workspaces: 20, environments: 2, snapshots: 20, diskGb: 100, cpu: 8, memoryGb: 32 };
  expect(requestedDiffs(limit, { workspaces: 40 })).toEqual([{ dim: "workspaces", from: 20, to: 40 }]);
  expect(requestedDiffs(limit, { workspaces: 40, diskGb: 250 })).toEqual([
    { dim: "workspaces", from: 20, to: 40 },
    { dim: "diskGb", from: 100, to: 250 },
  ]);
  expect(requestedDiffs(limit, {})).toEqual([]);
});
