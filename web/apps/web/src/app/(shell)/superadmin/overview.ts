import type { Overview } from "@/lib/api";
import { attentionTone, type HistorySeries } from "@/lib/history";

/** Nothing needs a decision — the landing view's one branch between "queue plus alerts" and
 *  "just the fleet". Pure so the branch is checkable without a fetch. */
export function needsNothing(o: Pick<Overview, "pendingRequests" | "attention">): boolean {
  return o.pendingRequests.length === 0 && o.attention.length === 0;
}

export type Gauge = { used: number; limit: number; unit: string };

/** The node gauges arrive as ratios (`pool_used` etc., spec §A5) rather than as byte counts, so a
 *  region card renders percentage against 100. An unavailable series gets `limit: 0`, which `pct`
 *  answers 0 for — a region whose collector is down must not read as an idle one. */
function gauge(s: HistorySeries): Gauge {
  return s.available ? { used: Math.round(s.summary.last * 100), limit: 100, unit: "%" } : { used: 0, limit: 0, unit: "%" };
}

export function regionCapacity(pool: HistorySeries, cpu: HistorySeries, memory: HistorySeries) {
  return { pool: gauge(pool), cpu: gauge(cpu), memory: gauge(memory) };
}

export type AttentionFilter = "all" | "critical" | "warn";

/** The feed's All/Critical/Warning tabs. The filter is over the TONE, not the kind string: kinds
 *  are open (a new signal name ships without a web change) and `attentionTone` is already the one
 *  place that decides how loud a kind is, so filtering on the raw string would silently drop the
 *  rows nobody has enumerated yet. */
export function filterAttention<T extends { kind: string }>(items: T[], filter: AttentionFilter): T[] {
  if (filter === "all") return items;
  return items.filter((i) => attentionTone(i.kind) === filter);
}
