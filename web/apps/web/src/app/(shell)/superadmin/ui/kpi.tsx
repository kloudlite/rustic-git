import { cn } from "@/lib/utils";
import { sparkPath } from "@/lib/console";
import type { HistorySeries } from "@/lib/history";

/** The 7-day inline SVG under a KPI number. `preserveAspectRatio="none"` so one path definition
 *  fits whatever width the strip's grid gives the tile. */
export function Sparkline({ series, className }: { series: HistorySeries; className?: string }) {
  const d = sparkPath(
    series.series.map((p) => p.value),
    100,
    24,
  );
  if (!d) {
    // An unavailable series draws a rule, not a fake zero line: the tile's sub-line already says
    // why, and a flat line at the bottom would read as a real measurement of nothing.
    return <div className={cn("h-6 border-b border-dashed border-border", className)} aria-hidden />;
  }
  return (
    <svg viewBox="0 0 100 24" preserveAspectRatio="none" className={cn("h-6 w-full", className)} aria-hidden>
      <path
        d={d}
        fill="none"
        stroke="currentColor"
        strokeWidth={1.5}
        vectorEffect="non-scaling-stroke"
        className="text-primary"
      />
    </svg>
  );
}

export function KpiTile({
  label,
  value,
  sub,
  series,
}: {
  label: string;
  value: string | number;
  sub: string;
  series?: HistorySeries;
}) {
  return (
    <div className="flex min-w-0 flex-col gap-1 border border-border bg-card p-4">
      <p className="text-micro font-medium tracking-eyebrow text-muted-foreground uppercase">{label}</p>
      <p className="text-title font-semibold tabular-nums leading-title">{value}</p>
      {series && <Sparkline series={series} />}
      <p className="line-clamp-2 text-caption text-muted-foreground">{sub}</p>
    </div>
  );
}

/** Four or five tiles across at desktop, stacking down to one — the strip is the first thing on
 *  every screen and must not force a horizontal scroll on a laptop. `cols` is the tile count, so a
 *  four-tile screen fills the row rather than leaving a fifth column of empty page. */
export function KpiStrip({ children, cols = 5 }: { children: React.ReactNode; cols?: 4 | 5 }) {
  return (
    <div className={cn("grid grid-cols-1 gap-3 sm:grid-cols-2", cols === 4 ? "xl:grid-cols-4" : "xl:grid-cols-5")}>
      {children}
    </div>
  );
}
