import { DIMS, atLimit, dimLabel, percent, type QuotaReport } from "@/lib/quota";

/** What the owner is using, against what they may. Six rows because six dimensions can each be the
 *  one that blocks a create, and a single "80% full" would hide which. `source`, when given,
 *  names where the limit came from ("own quota" vs "team default") — the Owner detail page's use,
 *  where that distinction is the whole point of Set quota existing. */
export function QuotaBar({ report, source }: { report: QuotaReport; source?: string }) {
  return (
    <div className="grid gap-2">
      {DIMS.map((d) => (
        <div key={d} className="flex flex-col gap-1 text-sm">
          <div className="flex items-center justify-between text-caption">
            <span className="font-medium">{dimLabel(d)}</span>
            {source && <span className="text-muted-foreground">{source}</span>}
          </div>
          <div className="h-2 bg-muted" role="presentation">
            <div
              className={atLimit(report, d) ? "h-2 bg-destructive" : "h-2 bg-primary"}
              style={{ width: `${percent(report.used[d], report.limit[d])}%` }}
            />
          </div>
          <span className="tabular-nums text-caption text-muted-foreground">
            {report.used[d]} of {report.limit[d]}
          </span>
        </div>
      ))}
    </div>
  );
}
