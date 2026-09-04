import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { NavTabs } from "@/components/app/nav-tabs";
import { createRegionAction, rollWorkloadAction } from "../actions";
import { PageHeader } from "../page-header";
import { RollTable } from "../roll-table";
import { NodeBadge } from "../status-badge";

export const metadata: Metadata = { title: "Clusters" };

/** One panel per region — its nodes and its own agent DaemonSet/gateway — selected the same way
 *  the old cluster-settings tab did: a plain query param, so switching regions is an ordinary
 *  navigation and never a client fetch that could show one region's rows under another's URL. */
export default async function ClustersPage({
  searchParams,
}: {
  searchParams: Promise<{ region?: string }>;
}) {
  const { token } = await requireSuperadmin("/superadmin/clusters");
  const { region: qRegion } = await searchParams;

  const [regionsRes, workloadsRes, nodesRes] = await Promise.all([
    api.listRegions(token),
    api.listWorkloads(token),
    api.adminListNodes(token),
  ]);
  const regions = regionsRes.ok ? regionsRes.value : [];
  const workloads = workloadsRes.ok ? workloadsRes.value : [];
  const nodes = nodesRes.ok ? nodesRes.value : [];

  const region = qRegion && regions.some((r) => r.id === qRegion) ? qRegion : regions[0]?.id;

  return (
    <div className="space-y-8">
      <PageHeader title="Clusters" purpose="Every region, its nodes, and its per-region agent and gateway." />

      <ul className="divide-y divide-border border border-border bg-card">
        {regions.length === 0 ? (
          <li className="px-4 py-8 text-center text-sm2 text-muted-foreground">No regions yet — add one below.</li>
        ) : (
          regions.map((rg) => (
            <li key={rg.id} className="flex items-center justify-between px-4 py-3 text-sm2">
              <span className="font-medium">{rg.id}</span>
              <span className="text-muted-foreground">{rg.status}</span>
            </li>
          ))
        )}
      </ul>

      <form action={createRegionAction} className="flex items-end gap-3 border border-border bg-card p-4">
        <label className="grid gap-1 text-sm2">
          Id
          <Input name="id" required className="h-8" />
        </label>
        <label className="grid gap-1 text-sm2">
          Name
          <Input name="name" required className="h-8" />
        </label>
        <Button type="submit" size="sm">
          Add region
        </Button>
      </form>

      {region && (
        <div className="space-y-6">
          <NavTabs
            aria-label="Region"
            activeHref={`/superadmin/clusters?region=${region}`}
            tabs={regions.map((r) => ({ href: `/superadmin/clusters?region=${r.id}`, label: r.id, exact: true }))}
          />

          <div>
            <h2 className="mb-2 text-sm2 font-medium">Nodes</h2>
            {/* ponytail: nodes aren't tagged by region in `GET /admin/nodes`, so every region's
                panel shows the whole fleet — narrow this once the backend adds a region field. */}
            <ul className="divide-y divide-border border border-border bg-card">
              {nodes.length === 0 ? (
                <li className="px-4 py-8 text-center text-sm2 text-muted-foreground">No nodes reported.</li>
              ) : (
                nodes.map((n) => (
                  <li key={n.name} className="flex items-center justify-between gap-3 px-4 py-3 text-sm2">
                    <span className="font-medium">{n.name}</span>
                    <NodeBadge n={n} />
                  </li>
                ))
              )}
            </ul>
          </div>

          <div>
            <h2 className="mb-2 text-sm2 font-medium">Workloads</h2>
            <RollTable workloads={workloads.filter((w) => w.scope === region)} onRoll={rollWorkloadAction} />
          </div>
        </div>
      )}
    </div>
  );
}
