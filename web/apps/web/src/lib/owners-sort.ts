import { DIMS, type QuotaDim } from "@/lib/quota";
import type { OwnerDetail, OwnerRow } from "@/lib/api";
import { pct } from "@/lib/console";

/** The dimension an owner will hit FIRST. A zero limit is skipped rather than treated as full:
 *  an unallocated dimension is not pressure, and counting it would sort every idle owner to the
 *  top of a table whose whole job is showing who is about to hit a wall. */
export function tightest(row: OwnerRow): { dim: QuotaDim; used: number; limit: number; percent: number } {
  let best = { dim: DIMS[0] as QuotaDim, used: 0, limit: 0, percent: 0 };
  for (const d of DIMS) {
    const limit = row.limit[d] ?? 0;
    if (limit <= 0) continue;
    const used = row.used[d] ?? 0;
    const percent = pct(used, limit);
    if (percent >= best.percent) best = { dim: d, used, limit, percent };
  }
  return best;
}

export function byTightest(rows: OwnerRow[]): OwnerRow[] {
  return [...rows].sort((a, b) => tightest(b).percent - tightest(a).percent);
}

/** The `own` / `default` chip on the owner detail's 3×2 grid — "where did this limit come from"
 *  was the question v1's flat six bars never answered.
 *
 *  `dim` is in the signature because the answer is per-dimension the moment the api can say so;
 *  today `OwnerDetail` carries ONE `source` for the whole owner (a `Quota` object replaces all
 *  six limits at once, `adminWriteQuota`), so every dimension reports the same word rather than
 *  the web inventing a field the api never sent. */
// eslint-disable-next-line @typescript-eslint/no-unused-vars
export function limitSource(detail: OwnerDetail, dim: QuotaDim): "own" | "default" {
  return detail.source;
}
