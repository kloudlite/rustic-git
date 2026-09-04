import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { fmt } from "@/lib/settings";
import { parseDecommissionStatus } from "@/lib/clusters";
import { rollWorkloadAction } from "../../actions";
import { RollTable } from "../../roll-table";
import { RegionStatusBadge } from "../../status-badge";
import { RegionStatusToggle } from "../region-status";
import { NodeActions } from "../node-actions";
import { DrainRefresh } from "../drain-refresh";

export async function generateMetadata({ params }: { params: Promise<{ region: string }> }): Promise<Metadata> {
  const { region } = await params;
  return { title: region };
}

/** The decommission cell — the sticky `drained <time>` or the four running/owned/copies/thin
 *  counters mid-drain, straight from the agent's own annotation (`lib/clusters.ts`). */
function DecommissionCell({ status }: { status: string | null }) {
  const p = parseDecommissionStatus(status);
  if (p.kind === "none") return <span className="text-muted-foreground">—</span>;
  if (p.kind === "drained") return <span className="text-caption">drained {p.at}</span>;
  return (
    <span className="text-caption text-warning">
      draining · running {p.running} · owned {p.owned} · copies {p.copies} · thin {p.thin}
    </span>
  );
}

export default async function ClusterDetailPage({ params }: { params: Promise<{ region: string }> }) {
  const { region } = await params;
  const { token } = await requireSuperadmin(`/superadmin/clusters/${region}`);
  const r = await api.adminClusterDetail(region, token);
  if (!r.ok) {
    if (r.kind === "notFound") notFound();
    throw new Error(r.message);
  }
  const detail = r.value;
  const nodesReady = detail.nodes.filter((n) => n.ready).length;
  const anyDraining = detail.nodes.some((n) => n.decommission && parseDecommissionStatus(n.decommissionStatus).kind === "draining");

  return (
    <div className="space-y-8">
      <DrainRefresh anyDraining={anyDraining} />
      <div className="flex items-end justify-between gap-4">
        <div>
          <h1 className="flex items-center gap-2 text-base font-medium">
            {detail.region}
            <RegionStatusBadge status={detail.status} />
          </h1>
          <p className="text-sm2 text-muted-foreground">{detail.nodes.length} nodes</p>
        </div>
        <RegionStatusToggle region={detail.region} status={detail.status} />
      </div>

      <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
        <div className="border border-border bg-card p-4">
          <div className="text-caption text-muted-foreground">Nodes</div>
          <div className="text-lg font-medium tabular-nums">{nodesReady} / {detail.nodes.length} ready</div>
          {detail.nodes.some((n) => n.decommission) && (
            <div className="text-caption text-warning">{detail.nodes.filter((n) => n.decommission).length} draining</div>
          )}
        </div>
        <div className="border border-border bg-card p-4">
          <div className="text-caption text-muted-foreground">Live working copies</div>
          <div className="text-lg font-medium tabular-nums">{detail.nodes.reduce((sum, n) => sum + n.workingCopies, 0)}</div>
        </div>
        <div className="border border-border bg-card p-4">
          <div className="text-caption text-muted-foreground">Cluster settings</div>
          <div className="text-sm2">
            <Link href="/superadmin/configuration" className="text-primary">Configuration</Link>
          </div>
        </div>
      </div>

      <div>
        <h2 className="mb-2 text-sm2 font-medium">Nodes</h2>
        <div className="overflow-x-auto border border-border bg-card">
          <table className="w-full text-sm2">
            <thead>
              <tr className="border-b border-border text-left text-caption text-muted-foreground">
                <th className="px-3 py-2 font-medium">Node</th>
                <th className="px-3 py-2 font-medium">Ready</th>
                <th className="px-3 py-2 font-medium">Hosted</th>
                <th className="px-3 py-2 font-medium">Replicas</th>
                <th className="px-3 py-2 font-medium">Decommission</th>
                <th className="px-3 py-2 font-medium" />
              </tr>
            </thead>
            <tbody className="divide-y divide-border">
              {detail.nodes.map((n) => (
                <tr key={n.name}>
                  <td className="px-3 py-2 font-mono text-caption">{n.name}</td>
                  <td className="px-3 py-2">{n.ready ? "ready" : "not ready"}</td>
                  <td className="px-3 py-2 tabular-nums">{n.workingCopies}</td>
                  <td className="px-3 py-2 tabular-nums">{n.replicasHeld}</td>
                  <td className="px-3 py-2"><DecommissionCell status={n.decommissionStatus} /></td>
                  <td className="px-3 py-2">
                    <NodeActions region={detail.region} node={n.name} decommission={n.decommission} decommissionStatus={n.decommissionStatus} />
                  </td>
                </tr>
              ))}
              {detail.nodes.length === 0 && (
                <tr>
                  <td colSpan={6} className="px-3 py-8 text-center text-muted-foreground">No nodes reported.</td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div>
        <h2 className="mb-2 text-sm2 font-medium">Workloads</h2>
        <RollTable workloads={detail.workloads} onRoll={rollWorkloadAction} />
      </div>

      <div>
        <div className="mb-2 flex items-center justify-between">
          <h2 className="text-sm2 font-medium">Settings</h2>
          <Link href="/superadmin/configuration" className="text-caption text-primary">Edit in Configuration</Link>
        </div>
        <div className="border border-border bg-card p-4">
          {Object.keys(detail.settings).length === 0 ? (
            <p className="text-sm2 text-muted-foreground">No cluster settings object for this region — riding env and defaults.</p>
          ) : (
            <dl className="grid grid-cols-2 gap-x-6 gap-y-1 text-sm2 sm:grid-cols-3">
              {Object.entries(detail.settings).map(([k, v]) => (
                <div key={k} className="flex justify-between gap-2 border-b border-border py-1 last:border-0">
                  <dt className="text-muted-foreground">{k}</dt>
                  <dd className="tabular-nums">{fmt(v)}</dd>
                </div>
              ))}
            </dl>
          )}
        </div>
      </div>
    </div>
  );
}
