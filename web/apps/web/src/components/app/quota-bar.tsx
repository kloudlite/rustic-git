import { DIMS, atLimit, dimLabel, percent, type QuotaReport } from "@/lib/quota";

/** What the owner is using, against what they may. Six rows because six dimensions can each be the
 *  one that blocks a create, and a single "80% full" would hide which. */
export function QuotaBar({ report }: { report: QuotaReport }) {
  return (
    <div className="grid gap-2">
      {DIMS.map((d) => (
        <div key={d} className="grid grid-cols-[8rem_1fr_6rem] items-center gap-3 text-sm">
          <span className="text-muted-foreground">{dimLabel(d)}</span>
          <div className="h-2 bg-muted" role="presentation">
            <div
              className={atLimit(report, d) ? "h-2 bg-destructive" : "h-2 bg-primary"}
              style={{ width: `${percent(report.used[d], report.limit[d])}%` }}
            />
          </div>
          <span className="tabular-nums text-right">
            {report.used[d]} / {report.limit[d]}
          </span>
        </div>
      ))}
    </div>
  );
}
