import type { SloJourneyStage, SloRunState, SloStatus, SloStep } from "@/lib/api";
import type { Tone } from "@/lib/console";

/** How much of the error budget is left, as the console words it. Both numbers from the api are
 *  COUNTS of bad samples — what the window can afford and what is still unspent, signed — so the
 *  percentage is their ratio. A 100 % target affords nothing: there is no ratio, and the row
 *  says so in bad samples instead. `null` is no sample at all in the window. */
export function budgetLabel(left: number | null, budget: number): string {
  if (left == null) return "—";
  if (budget <= 0) {
    const bad = Math.round(-left);
    return bad === 0 ? "no budget · 0 bad" : `no budget · ${bad} bad`;
  }
  const p = Math.round((Math.abs(left) / budget) * 100);
  return left < 0 ? `${p} % over` : `${p} % left`;
}

/** The share of the budget spent, 0–100, for the bar: full at the wall, never past it. */
export function budgetSpentPct(left: number | null, budget: number): number {
  if (left == null) return 0;
  if (budget <= 0) return left < 0 ? 100 : 0;
  return Math.min(100, Math.max(0, Math.round((1 - left / budget) * 100)));
}

/** A burn rate is a multiple of the budget's own spend rate: 1× is exactly on budget. `null` is a
 *  window this SLO does not have (a weekly SLO has no 1 h window) or one with no samples. */
export function burnLabel(rate: number | null): string {
  return rate == null ? "—" : `${Math.round(rate * 10) / 10}×`;
}

/** The burn columns are labelled from each SLO's own windows, not from a fixed "1 h / 6 h": the
 *  catalogue mixes per-request SLOs with weekly ones, and a column header naming a window half
 *  the rows do not have is a lie in the header rather than in the cells. */
export function windowLabel(secs: number): string {
  const units: [number, string][] = [
    [604800, "w"],
    [86400, "d"],
    [3600, "h"],
    [60, "m"],
  ];
  for (const [size, unit] of units) {
    if (secs >= size) return `${Math.round(secs / size)} ${unit}`;
  }
  return `${secs} s`;
}

/** Order the table sorts within a feature (spec §Console). Burning first: it is the one state
 *  that is still changing and still actionable. */
const STATE_ORDER: Record<SloStatus["state"], number> = { burning: 0, breaching: 1, unknown: 2, ok: 3 };

export function sloTone(state: SloStatus["state"]): Tone {
  return state === "breaching" ? "critical" : state === "burning" ? "warn" : state === "ok" ? "ok" : "neutral";
}

/** Group by feature in CATALOGUE order — the api returns the catalogue's own order, so first
 *  appearance is that order and no second sort key is needed. Within a feature the rows sort by
 *  state, so what is burning is at the top of its own group rather than buried mid-feature. */
export function groupByFeature(slos: SloStatus[]): { feature: string; slos: SloStatus[] }[] {
  const groups: { feature: string; slos: SloStatus[] }[] = [];
  for (const s of slos) {
    const g = groups.find((x) => x.feature === s.feature);
    if (g) g.slos.push(s);
    else groups.push({ feature: s.feature, slos: [s] });
  }
  for (const g of groups) g.slos.sort((a, b) => STATE_ORDER[a.state] - STATE_ORDER[b.state]);
  return groups;
}

export function runTone(state: SloRunState): Tone {
  return state === "failed" ? "critical" : state === "passed" ? "ok" : "info";
}

/** A duration, in the largest unit that still says something: a step is milliseconds, a stage is
 *  seconds and a whole run is minutes, and one column has to hold all three. Fixed shapes
 *  (`3.2 s`, `1 m 04 s`) so the column stays a column when the numbers change under a poll. */
export function msLabel(ms: number | null): string {
  if (ms == null || !Number.isFinite(ms)) return "—";
  if (ms < 1_000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1_000).toFixed(1)} s`;
  const m = Math.floor(ms / 60_000);
  return `${m} m ${String(Math.round((ms - m * 60_000) / 1_000)).padStart(2, "0")} s`;
}

/** The latency ceiling out of a catalogue target, which the api renders as a sentence
 *  ("95 % ≤ 2000 ms"). An availability-only SLO has none, and then a step's ms is a fact with
 *  nothing to be over — never a red number for want of a target. */
export function targetMs(target: string | undefined): number | null {
  const m = /≤\s*(\d+)\s*ms/.exec(target ?? "");
  return m ? Number(m[1]) : null;
}

export type StageState = "pending" | "running" | "passed" | "failed" | "skipped";

export type StepNode = { id: string; step: SloStep | null };

export type StageNode = {
  name: string;
  state: StageState;
  steps: StepNode[];
  /** Steps that ran and were good, over what the stage will report in total. */
  ok: number;
  total: number;
  ms: number;
  /** The first step's timestamp, which is what a running stage clocks its elapsed from. */
  startedTs: string | null;
};

/** The journey as a tree: every stage the suite will walk, every step it will report, and the
 *  ones that have happened filled in. Built from the JOURNEY rather than from the steps, so the
 *  reader sees what is still to come instead of a list that grows a row at a time.
 *
 *  Stage state is derived, never reported: a stage with a failed step failed; one whose steps are
 *  all skipped was skipped; one that has reported some but not all of its ids is still running;
 *  an empty stage after a failure is skipped, and otherwise has not started. That last rule is
 *  why this needs no run state — a finished failed run and a running one read alike, correctly. */
export function treeOf(journey: SloJourneyStage[], steps: SloStep[]): StageNode[] {
  const byStage = new Map<string, SloStep[]>();
  for (const s of steps) {
    const at = byStage.get(s.stage);
    if (at) at.push(s);
    else byStage.set(s.stage, [s]);
  }
  let failedEarlier = false;
  return journey.map((stage) => {
    const reported = byStage.get(stage.name) ?? [];
    // Extra ids the catalogue does not list still belong to the stage that reported them: a
    // console that drops a step the probe measured hides the one thing worth seeing.
    const ids = [...stage.ids, ...reported.map((s) => s.slo_id).filter((id) => !stage.ids.includes(id))];
    const seen = new Map(reported.map((s) => [s.slo_id, s]));
    const failed = reported.some((s) => !s.ok && !s.skipped);
    const state: StageState = failed
      ? "failed"
      : reported.length === 0
        ? failedEarlier
          ? "skipped"
          : "pending"
        : reported.every((s) => s.skipped)
          ? "skipped"
          : reported.length < ids.length
            ? "running"
            : "passed";
    failedEarlier = failedEarlier || failed;
    return {
      name: stage.name,
      state,
      steps: ids.map((id) => ({ id, step: seen.get(id) ?? null })),
      ok: reported.filter((s) => s.ok && !s.skipped).length,
      total: ids.length,
      ms: reported.reduce((a, s) => a + s.ms, 0),
      startedTs: reported.length > 0 ? reported[0].ts : null,
    };
  });
}

/** Steps done over steps the journey holds — the number the progress bar draws. A skipped step is
 *  done: nothing more will happen to it. */
export function progressOf(tree: StageNode[]): { done: number; total: number } {
  let done = 0;
  let total = 0;
  for (const s of tree) {
    total += s.steps.length;
    done += s.steps.filter((x) => x.step).length;
  }
  return { done, total };
}
