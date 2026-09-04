import Link from "next/link";
import type { AdminClusterRow } from "@/lib/api";
import { CapacityBar } from "./ui/capacity-bar";
import { Pill } from "./ui/pill";
import type { Gauge } from "./overview";

/** A gauge whose series was unavailable has `limit: 0`. Drawing it through `CapacityBar` would
 *  paint an empty track at 0%, which is exactly what a genuinely idle region looks like — so the
 *  missing case gets its own dashed rule instead, matching `Sparkline`'s unavailable state. */
function Bar({ label, gauge }: { label: string; gauge: Gauge }) {
  if (gauge.limit === 0) {
    return (
      <div className="min-w-0">
        <div className="h-1.5 w-full border-b border-dashed border-border" aria-hidden />
        <div className="mt-1 flex items-baseline justify-between gap-2 text-caption text-muted-foreground">
          <span>history unavailable</span>
          <span>{label}</span>
        </div>
      </div>
    );
  }
  return <CapacityBar used={gauge.used} limit={gauge.limit} unit={gauge.unit} label={label} />;
}

export type RegionGauges = { pool: Gauge; cpu: Gauge; memory: Gauge };

/** One card per region: how much of the fleet is actually up, then the three node gauges the
 *  history layer reports for it. `status` is an open string from the api, so only the exact word
 *  "active" is treated as good — an unrecognised one reads as something to look at. */
export function RegionCapacityCard({ region, gauges }: { region: AdminClusterRow; gauges: RegionGauges }) {
  return (
    <div className="flex flex-col gap-2 border-b border-border pb-4 last:border-0 last:pb-0">
      <div className="flex items-center gap-2">
        <span className="min-w-0 truncate font-mono text-sm2 font-medium">{region.region}</span>
        <Pill tone={region.status === "active" ? "ok" : "warn"}>{region.status}</Pill>
      </div>
      <p className="text-caption tabular-nums text-muted-foreground">
        {region.nodesReady} of {region.nodesTotal} nodes ready · {region.agentsReady} / {region.agentsDesired} agents
      </p>
      <Bar label="Disk pool" gauge={gauges.pool} />
      <Bar label="CPU" gauge={gauges.cpu} />
      <Bar label="Memory" gauge={gauges.memory} />
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-caption tabular-nums text-muted-foreground">
          {region.workingCopies} live working copies
        </span>
        <Link
          href={`/superadmin/clusters/${encodeURIComponent(region.region)}`}
          className="text-caption text-primary underline-offset-4 hover:underline"
        >
          Open
        </Link>
      </div>
    </div>
  );
}
