import { describe, expect, test } from "bun:test";
import { atLimit, dimFromRefusal, dimLabel, percent, type QuotaReport } from "@/lib/quota";

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
