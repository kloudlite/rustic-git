import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { PageHeader } from "../page-header";
import { KpiStrip, KpiTile } from "../ui/kpi";
import { EmptyState } from "../ui/data-table";
import { AddRegionForm } from "./add-region-form";
import { RegionCard } from "./region-card";

export const metadata: Metadata = { title: "Clusters" };

/** `deploy/pin.sh` pins `repo:tag@sha256:hex`; the card wants the readable half. */
function tagOf(image: string | null): string | null {
  const ref = image?.split("@")[0];
  return ref ? (ref.split(":")[1] ?? ref) : null;
}

/** One section per region — everything `Clusters.dc.html` asks for without a second click: node
 *  dots, the disk pool, live working copies, the agent image and the settings chip. */
export default async function ClustersPage() {
  const { token } = await requireSuperadmin("/superadmin/clusters");
  const [r, workloadsR, copies] = await Promise.all([
    api.adminClusters(token),
    api.listWorkloads(token),
    api.adminSeries("live_workspaces", { range: "7d", step: "1d" }, token),
  ]);
  const rows = r.ok ? r.value : [];
  const workloads = workloadsR.ok ? workloadsR.value : [];

  const nodesReady = rows.reduce((n, c) => n + c.nodesReady, 0);
  const nodesTotal = rows.reduce((n, c) => n + c.nodesTotal, 0);
  const agentsReady = rows.reduce((n, c) => n + c.agentsReady, 0);
  const agentsDesired = rows.reduce((n, c) => n + c.agentsDesired, 0);
  const draining = rows.reduce((n, c) => n + c.draining, 0);
  const workingCopies = rows.reduce((n, c) => n + c.workingCopies, 0);
  const active = rows.filter((c) => c.status === "active").length;

  return (
    <div className="space-y-4">
      <AutoRefresh />
      <PageHeader title="Clusters" purpose="Every region this install runs in, and how much room is left in each." />
      {!r.ok && <p className="text-sm2 text-destructive">{r.message}</p>}

      <KpiStrip>
        <KpiTile label="Regions" value={rows.length} sub={`${active} accepting new work`} />
        <KpiTile label="Nodes ready" value={`${nodesReady} / ${nodesTotal}`} sub={`${draining} draining · ${nodesTotal - nodesReady} not ready`} />
        <KpiTile label="Agents ready" value={`${agentsReady} / ${agentsDesired}`} sub="one agent per btrfs-capable node" />
        <KpiTile label="Draining" value={draining} sub={draining === 0 ? "no node is being retired" : "running work keeps running"} />
        <KpiTile
          label="Live working copies"
          value={workingCopies}
          sub={copies.available ? "workspaces and environments" : "history unavailable"}
          series={copies}
        />
      </KpiStrip>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        {rows.map((c) => (
          <RegionCard
            key={c.region}
            region={c}
            token={token}
            agentImage={tagOf(workloads.find((w) => w.scope === c.region && w.kind === "daemonset")?.image ?? null)}
          />
        ))}
      </div>
      {rows.length === 0 && (
        <EmptyState>No region is registered yet — add the first one below to start placing work.</EmptyState>
      )}

      <AddRegionForm />
    </div>
  );
}
