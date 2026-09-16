import { when } from "@/lib/time";

/** The six dimensions a quota has, in the order the bar shows them. The words are the api's own
 *  field names, so a 409 naming one is directly a key here — one vocabulary, not two. */
export const DIMS = ["workspaces", "environments", "snapshots", "diskGb", "cpu", "memoryGb"] as const;
export type QuotaDim = (typeof DIMS)[number];

export type QuotaReport = {
  owner: string;
  limit: Record<QuotaDim, number>;
  used: QuotaUsage;
  /** Disk stated on its own, because it is the one dimension that is MEASURED rather than
   *  counted: `usedGb` is the sum of what the volumes occupy, stamped on the sync beat, so it is
   *  only ever true "as of `usedAt`" (absent until some volume has been stamped).
   *  `usedGb` is the same number as `used.diskGb` — the api sends both. */
  disk?: { usedGb: number; limitGb: number; usedAt?: string | null };
};

/** Usage carries one extra field beside the six counts: when the disk figure was measured
 *  (`quota::Usage::disk_used_at`, the OLDEST stamp behind the sum — the whole number is only as
 *  fresh as its stalest part). Absent when no volume has been stamped yet. */
export type QuotaUsage = Record<QuotaDim, number> & { diskUsedAt?: string | null };

export function dimLabel(d: QuotaDim): string {
  return {
    workspaces: "Workspaces",
    environments: "Environments",
    snapshots: "Snapshots",
    diskGb: "Disk",
    cpu: "CPU",
    memoryGb: "Memory",
  }[d];
}

/** The unit a limit is counted in, shown beside the number so "100" never has to be guessed at:
 *  the api's field names carry it (`diskGb`) but the label does not. */
export function dimUnit(d: QuotaDim): string {
  return { workspaces: "", environments: "", snapshots: "", diskGb: "GB", cpu: "cores", memoryGb: "GB" }[d];
}

/** A whole percentage for the bar's width. A zero limit reads FULL rather than NaN — it is a
 *  dimension nobody may use — and over-quota clamps, because /v1 is read-then-write and a limit
 *  can be lowered under existing use. */
export function percent(used: number, limit: number): number {
  if (limit <= 0) return 100;
  return Math.min(100, Math.round((used / limit) * 100));
}

/** What a disk figure is worth saying next to it: when it was measured. `null` — nothing stamped
 *  yet — reads as "not measured yet", never as "just now", because a missing stamp usually means
 *  a volume the sync beat has not reached rather than a fresh one. */
export function asOf(usedAt?: string | null): string {
  return usedAt ? `as of ${when(Date.parse(usedAt))}` : "not measured yet";
}

export function atLimit(r: QuotaReport, d: QuotaDim): boolean {
  return r.used[d] >= r.limit[d];
}

/** The smallest (limit - used)/limit across the six dimensions — the Owners list's sort key,
 *  mirroring the api's own `tightest_ratio` exactly so the two never disagree about ordering.
 *  A zero limit reads maximally tight (negative infinity), never "infinite headroom". */
export function tightestRatio(limit: Record<QuotaDim, number>, used: Record<QuotaDim, number>): number {
  return DIMS.reduce((min, d) => {
    const l = limit[d];
    const ratio = l <= 0 ? -Infinity : (l - used[d]) / l;
    return Math.min(min, ratio);
  }, Infinity);
}

/** One row per dimension the request touches: the owner's current limit next to what was asked.
 *  A dimension not in `requested` never appears — the request didn't touch it. */
export function requestedDiffs(
  limit: Record<QuotaDim, number>,
  requested: Partial<Record<QuotaDim, number>>,
): { dim: QuotaDim; from: number; to: number }[] {
  return DIMS.filter((d) => requested[d] !== undefined).map((d) => ({ dim: d, from: limit[d], to: requested[d]! }));
}

/** The dimension a 409 named, so the request form opens on the field that blocked them.
 *  The sentence is fixed by the api (`quota::refuse`); anything else is not a quota refusal. */
export function dimFromRefusal(message: string): QuotaDim | null {
  const word = message.split(":")[0]?.trim();
  return (DIMS as readonly string[]).includes(word) && message.includes("request more under Quota")
    ? (word as QuotaDim)
    : null;
}
