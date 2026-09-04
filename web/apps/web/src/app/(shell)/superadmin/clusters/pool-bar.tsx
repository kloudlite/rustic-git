import type { HistorySeries } from "@/lib/history";
import { CapacityBar } from "../ui/capacity-bar";

/** Disk pressure comes from the `pool_used` history series (a ratio in [0,1] per region) — no
 *  CRD carries a pool size, so with history down there is no number to fall back on. The dashed
 *  rule says so rather than drawing a 0% bar, which would read as an empty pool. */
export function PoolBar({ series, label = "of the pool" }: { series: HistorySeries; label?: string }) {
  if (!series.available) {
    return (
      <div className="min-w-0">
        <div className="h-1.5 w-full border-b border-dashed border-border" aria-hidden />
        <p className="mt-1 text-caption text-muted-foreground">history unavailable</p>
      </div>
    );
  }
  return <CapacityBar used={Math.round(series.summary.last * 100)} limit={100} unit="%" label={label} />;
}
