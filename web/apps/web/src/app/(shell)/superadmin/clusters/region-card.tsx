import Link from "next/link";
import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import type { AdminClusterRow } from "@/lib/api";
import { settingsStatusTone } from "@/lib/clusters";
import { Section } from "../ui/section";
import { Pill } from "../ui/pill";
import { PoolBar } from "./pool-bar";
import { RegionStatusToggle } from "./region-status";

/** One region as a `Section` (`Clusters.dc.html`): ready dots, the disk-pool bar, live working
 *  copies, the agent image tag and the ClusterSettings chip — everything without a second click. */
export async function RegionCard({
  region,
  agentImage,
  token,
}: {
  region: AdminClusterRow;
  agentImage: string | null;
  token: string;
}) {
  // Each card reads its own `pool_used` — the only place a disk ratio exists — so the cards fetch
  // side by side rather than the page awaiting a second serialised round trip for all of them.
  const pool = await api.adminSeries("pool_used", { range: "7d", step: "1d", region: region.region }, token);
  const href = `/superadmin/clusters/${encodeURIComponent(region.region)}`;
  const settings = settingsStatusTone(region.settingsStatus);
  return (
    <Section
      eyebrow="Region"
      title={region.region}
      toolbar={
        <>
          <Pill tone={region.status === "active" ? "ok" : "warn"}>{region.status}</Pill>
          <RegionStatusToggle region={region.region} status={region.status} />
          <Link href={href} className="text-caption text-primary underline-offset-4 hover:underline">
            Open
          </Link>
        </>
      }
    >
      <div className="flex flex-col gap-3">
        <div className="flex items-center gap-1" aria-label={`${region.nodesReady} of ${region.nodesTotal} nodes ready`}>
          {/* One dot per node: a count says "3 of 4", the dots say WHICH shape the region is in
              at a glance across a row of regions. */}
          {Array.from({ length: region.nodesTotal }, (_, i) => (
            <span key={i} className={cn("size-2", i < region.nodesReady ? "bg-success" : "bg-destructive")} />
          ))}
          <span className="ml-2 text-caption tabular-nums text-muted-foreground">
            {region.nodesReady} of {region.nodesTotal} nodes ready · {region.agentsReady} / {region.agentsDesired} agents ready
            {region.draining > 0 && ` · ${region.draining} draining`}
          </span>
        </div>
        <div>
          <p className="text-caption text-muted-foreground">Disk pool</p>
          <PoolBar series={pool} />
        </div>
        <div className="flex flex-wrap items-center gap-3 text-caption text-muted-foreground">
          <span className="tabular-nums">{region.workingCopies} live working copies</span>
          {/* The agent's image is the workloads row's, not a field on the region — one source for
              the tag the Cluster page also rolls. */}
          <span className="font-mono">{agentImage ?? "—"}</span>
          <Pill tone={settings === "present" ? "ok" : settings === "unknown" ? "neutral" : "warn"}>
            {region.settingsStatus}
          </Pill>
        </div>
      </div>
    </Section>
  );
}
