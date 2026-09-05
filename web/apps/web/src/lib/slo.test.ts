import { describe, expect, test } from "bun:test";
import type { SloStatus, SloStep } from "@/lib/api";
import { budgetLabel, burnLabel, groupByFeature, stagesOf, windowLabel } from "@/lib/slo";

const slo = (id: string, feature: string, state: SloStatus["state"]): SloStatus => ({
  id,
  feature,
  sli: id,
  target: "99.9 %",
  suite: "fast",
  attainment_30d: 0.999,
  total_30d: 100,
  budget_left: 0.5,
  burn_short: null,
  burn_long: null,
  window_short_secs: 3600,
  window_long_secs: 21600,
  last: null,
  state,
});

const step = (slo_id: string, stage: string): SloStep => ({
  slo_id,
  stage,
  ts: "2026-09-05T00:00:00Z",
  ok: true,
  ms: 12,
  skipped: false,
  detail: "",
});

describe("budgetLabel", () => {
  test("reads as what is left", () => {
    expect(budgetLabel(0.12)).toBe("12 % left");
  });
  // A breaching SLO has spent more budget than it had; "-30 % left" would read as a bug.
  test("says an overspend in its own words", () => {
    expect(budgetLabel(-0.3)).toBe("30 % over");
  });
  test("no sample is a dash, never 0 %", () => {
    expect(budgetLabel(null)).toBe("—");
  });
});

describe("burnLabel", () => {
  test("is a multiple of the budget's own rate, to one decimal", () => {
    expect(burnLabel(1.44)).toBe("1.4×");
    expect(burnLabel(null)).toBe("—");
  });
});

describe("windowLabel", () => {
  test("names the window in its largest whole unit", () => {
    expect(windowLabel(3600)).toBe("1 h");
    expect(windowLabel(21600)).toBe("6 h");
    expect(windowLabel(2419200)).toBe("4 w");
  });
});

describe("groupByFeature", () => {
  test("keeps the catalogue's feature order", () => {
    const g = groupByFeature([slo("a", "Git", "ok"), slo("b", "Registry", "ok"), slo("c", "Git", "ok")]);
    expect(g.map((x) => x.feature)).toEqual(["Git", "Registry"]);
    expect(g[0].slos.map((s) => s.id)).toEqual(["a", "c"]);
  });
  test("sorts burning first inside a feature", () => {
    const g = groupByFeature([slo("ok", "Git", "ok"), slo("un", "Git", "unknown"), slo("burn", "Git", "burning")]);
    expect(g[0].slos.map((s) => s.id)).toEqual(["burn", "un", "ok"]);
  });
});

describe("stagesOf", () => {
  test("groups consecutive steps by stage", () => {
    const g = stagesOf([step("a", "1 · Edge"), step("b", "1 · Edge"), step("c", "2 · Git")]);
    expect(g.map((s) => s.stage)).toEqual(["1 · Edge", "2 · Git"]);
    expect(g[0].steps.map((s) => s.slo_id)).toEqual(["a", "b"]);
  });
  // A run that returns to a stage ran it twice; folding them would put minutes-apart steps in one row.
  test("a stage revisited later is its own row", () => {
    const g = stagesOf([step("a", "1 · Edge"), step("b", "2 · Git"), step("c", "1 · Edge")]);
    expect(g.map((s) => s.stage)).toEqual(["1 · Edge", "2 · Git", "1 · Edge"]);
  });
});
