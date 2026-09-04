import { cn } from "@/lib/utils";
import { capacityTone, pct, type Tone } from "@/lib/console";

const FILL: Record<Tone, string> = {
  ok: "bg-primary",
  warn: "bg-warning",
  critical: "bg-destructive",
  info: "bg-primary",
  neutral: "bg-muted-foreground",
};

/** Capacity is never a bare number (design README): a 6 px track with `used / limit unit` under
 *  it, right-aligned, so the same shape reads the same on a KPI tile, a queue row and the owner
 *  grid. `label` overrides the generated one where the mockup words it differently
 *  (`6.4 / 8 TB`). */
export function CapacityBar({
  used,
  limit,
  unit,
  label,
  className,
}: {
  used: number;
  limit: number;
  unit: string;
  label?: string;
  className?: string;
}) {
  const p = pct(used, limit);
  const tone = capacityTone(used, limit);
  return (
    <div className={cn("min-w-0", className)}>
      <div className="h-1.5 w-full bg-muted" role="img" aria-label={`${p}% of ${limit} ${unit} in use`}>
        <div className={cn("h-full", FILL[tone])} style={{ width: `${p}%` }} />
      </div>
      <div className="mt-1 flex items-baseline justify-between gap-2 text-caption tabular-nums text-muted-foreground">
        <span>{p}%</span>
        <span>{label ?? `${used} / ${limit} ${unit}`}</span>
      </div>
    </div>
  );
}
