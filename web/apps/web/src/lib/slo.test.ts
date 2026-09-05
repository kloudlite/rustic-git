import { describe, expect, test } from "bun:test";
import type { SloStatus, SloStep } from "@/lib/api";
import { budgetLabel, burnLabel, groupByFeature, msLabel, progressOf, targetMs, treeOf, windowLabel } from "@/lib/slo";

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

describe("msLabel", () => {
  test("says a step in ms, a stage in seconds and a run in minutes", () => {
    expect(msLabel(412)).toBe("412 ms");
    expect(msLabel(3_240)).toBe("3.2 s");
    expect(msLabel(64_000)).toBe("1 m 04 s");
    expect(msLabel(null)).toBe("—");
  });
});

describe("targetMs", () => {
  test("reads the latency ceiling out of a rendered target", () => {
    expect(targetMs("95 % ≤ 2000 ms")).toBe(2000);
    expect(targetMs("99.9 % ≤ 30000 ms")).toBe(30000);
  });
  // An availability-only SLO has no ceiling: a step's ms must never be red for want of a target.
  test("an availability target has no ceiling", () => {
    expect(targetMs("99.9 %")).toBeNull();
    expect(targetMs(undefined)).toBeNull();
  });
});

const JOURNEY = [
  { name: "0 · Boot", ids: [] },
  { name: "1 · Identity", ids: ["id.signin", "id.token.mint"] },
  { name: "2 · Git", ids: ["git.push.ok", "git.clone.p95"] },
  { name: "3 · Registry", ids: ["reg.push.ok"] },
];

describe("treeOf", () => {
  test("a stage nobody has reached yet is pending, with its steps already named", () => {
    const t = treeOf(JOURNEY, []);
    expect(t.map((s) => s.state)).toEqual(["pending", "pending", "pending", "pending"]);
    expect(t[1].steps.map((s) => s.id)).toEqual(["id.signin", "id.token.mint"]);
    expect(t[1].steps.every((s) => s.step === null)).toBe(true);
  });
  test("a stage half reported is running, and sums the ms it has", () => {
    const t = treeOf(JOURNEY, [step("id.signin", "1 · Identity"), step("id.token.mint", "1 · Identity"), step("git.push.ok", "2 · Git")]);
    expect(t.map((s) => s.state)).toEqual(["pending", "passed", "running", "pending"]);
    expect(t[1].ms).toBe(24);
    expect(t[1].ok).toBe(2);
  });
  // A failed run's remaining stages never ran; "pending" would read as a run still in flight.
  test("everything after a failure is skipped, not pending", () => {
    const failed = { ...step("git.push.ok", "2 · Git"), ok: false, detail: "500" };
    const t = treeOf(JOURNEY, [step("id.signin", "1 · Identity"), step("id.token.mint", "1 · Identity"), failed]);
    expect(t.map((s) => s.state)).toEqual(["pending", "passed", "failed", "skipped"]);
  });
  test("a stage whose steps were all skipped is skipped", () => {
    const sk = { ...step("reg.push.ok", "3 · Registry"), ok: false, skipped: true };
    expect(treeOf(JOURNEY, [sk])[3].state).toBe("skipped");
  });
  // An id the catalogue does not list still belongs to the stage that reported it.
  test("keeps a step the journey does not name", () => {
    const t = treeOf(JOURNEY, [step("git.extra", "2 · Git")]);
    expect(t[2].steps.map((s) => s.id)).toEqual(["git.push.ok", "git.clone.p95", "git.extra"]);
  });
});

describe("progressOf", () => {
  test("counts steps done over steps the journey holds", () => {
    expect(progressOf(treeOf(JOURNEY, []))).toEqual({ done: 0, total: 5 });
    expect(progressOf(treeOf(JOURNEY, [step("id.signin", "1 · Identity")]))).toEqual({ done: 1, total: 5 });
  });
});
