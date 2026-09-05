import type { SloStatus, SloStep } from "@/lib/api";
import type { Tone } from "@/lib/console";

/** How much of the error budget is left, as the console words it. The api reports a fraction and
 *  lets it go negative — a breaching SLO has spent more than it had — and "-30 % left" reads as a
 *  rendering bug, so an overspend is said in its own words instead. `null` is no sample at all in
 *  the window, which is never 0 %. */
export function budgetLabel(left: number | null): string {
  if (left == null) return "—";
  const p = Math.round(Math.abs(left) * 100);
  return left < 0 ? `${p} % over` : `${p} % left`;
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

/** The journey's stages, in the order the run walked them. Grouped by CONSECUTIVE stage rather
 *  than by name: a probe that comes back to a stage later ran it twice, and folding the two into
 *  one row would put steps next to each other that minutes apart. */
export function stagesOf(steps: SloStep[]): { stage: string; steps: SloStep[] }[] {
  const out: { stage: string; steps: SloStep[] }[] = [];
  for (const s of steps) {
    const tail = out[out.length - 1];
    if (tail && tail.stage === s.stage) tail.steps.push(s);
    else out.push({ stage: s.stage, steps: [s] });
  }
  return out;
}
