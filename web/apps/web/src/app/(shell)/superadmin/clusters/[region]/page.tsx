import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { fmt } from "@/lib/settings";
import { parseDecommissionStatus } from "@/lib/clusters";
import { nodeTone, nodeVerbs } from "@/lib/nodes";
import { rollWorkloadAction } from "../../actions";
import { RollTable } from "../../roll-table";
import { Section } from "../../ui/section";
import { KpiStrip, KpiTile } from "../../ui/kpi";
import { Pill } from "../../ui/pill";
import { DataTable, EmptyState, RowActions, Td, Th, Tr } from "../../ui/data-table";
import { RegionStatusToggle } from "../region-status";
import { NodeActions } from "../node-actions";
import { DrainRefresh } from "../drain-refresh";

export async function generateMetadata({ params }: { params: Promise<{ region: string }> }): Promise<Metadata> {
  const { region } = await params;
  return { title: region };
}

/** The word under the Ready column: `draining` is a third state, not a shade of ready — the node
 *  is up and serving while the agent's own beat empties it. */
function readyWord(n: api.AdminClusterNode): string {
  if (!n.ready) return "not ready";
  return n.decommission ? "draining" : "ready";
}

export default async function ClusterDetailPage({ params }: { params: Promise<{ region: string }> }) {
  const { region } = await params;
  const { token } = await requireSuperadmin(`/superadmin/clusters/${region}`);
  const [r, pool] = await Promise.all([
    api.adminClusterDetail(region, token),
    api.adminSeries("pool_used", { range: "7d", step: "1d", region }, token),
  ]);
  if (!r.ok) {
    if (r.kind === "notFound") notFound();
    throw new Error(r.message);
  }
  const detail = r.value;
  const nodesReady = detail.nodes.filter((n) => n.ready).length;
  const draining = detail.nodes.filter((n) => n.decommission);
  const anyDraining = draining.some((n) => parseDecommissionStatus(n.decommissionStatus).kind === "draining");
  const workingCopies = detail.nodes.reduce((sum, n) => sum + n.workingCopies, 0);
  const replicas = detail.nodes.reduce((sum, n) => sum + n.replicasHeld, 0);
  const thin = draining.reduce((sum, n) => {
    const p = parseDecommissionStatus(n.decommissionStatus);
    return sum + (p.kind === "draining" ? p.thin : 0);
  }, 0);

  return (
    <div className="space-y-4">
      <DrainRefresh anyDraining={anyDraining} />
      <div className="mb-2 flex items-end justify-between gap-4">
        <div>
          <p className="text-caption text-muted-foreground">
            <Link href="/superadmin/clusters" className="hover:underline">Clusters</Link> / {detail.region}
          </p>
          <h1 className="flex items-center gap-2 text-base font-medium">
            {detail.region}
            <Pill tone={detail.status === "active" ? "ok" : "warn"}>{detail.status}</Pill>
          </h1>
          <p className="text-sm2 text-muted-foreground">
            {detail.nodes.length} nodes · {workingCopies} live working copies · {replicas} replicas held
          </p>
        </div>
        <div className="flex items-center gap-2">
          <RegionStatusToggle region={detail.region} status={detail.status} />
          <Link href="/superadmin/configuration" className="inline-flex h-8 items-center border border-border px-3 text-sm2 font-medium hover:bg-muted">
            Edit settings
          </Link>
        </div>
      </div>

      <KpiStrip>
        <KpiTile
          label="Nodes ready"
          value={`${nodesReady} / ${detail.nodes.length}`}
          sub={draining.length === 0 ? "none draining" : `${draining.map((n) => n.name).join(", ")} draining`}
        />
        <KpiTile label="Live working copies" value={workingCopies} sub="workspaces and environments on these nodes" />
        <KpiTile label="Replicas held" value={replicas} sub="snapshot copies these nodes carry" />
        <KpiTile
          label="Disk pool"
          value={pool.available ? `${Math.round(pool.summary.last * 100)}%` : "—"}
          sub={pool.available ? "of the region pool in use" : "history unavailable"}
          series={pool}
        />
        <KpiTile
          label="Replicas thin"
          value={thin}
          sub={thin === 0 ? "every volume is at its replica count" : "below spec.replicas on a draining node"}
        />
      </KpiStrip>

      <Section eyebrow="Capacity" title="Nodes" count={detail.nodes.length} bare>
        {detail.nodes.length === 0 ? (
          <EmptyState>No node reported — check the agent DaemonSet is scheduled in this region.</EmptyState>
        ) : (
          <DataTable>
            <thead>
              <tr>
                <Th>Node</Th>
                <Th>Ready</Th>
                <Th>Decommission</Th>
                <Th numeric>Hosted</Th>
                <Th numeric>Replicas</Th>
                <Th />
              </tr>
            </thead>
            <tbody>
              {detail.nodes.map((n) => (
                <Tr key={n.name}>
                  <Td className="font-mono text-caption">{n.name}</Td>
                  <Td><Pill tone={nodeTone(n)}>{readyWord(n)}</Pill></Td>
                  {/* The agent's own annotation, verbatim: the counters are what a drain is
                      waiting for, and paraphrasing them has already hidden a stuck `thin`. */}
                  <Td className="font-mono text-caption">{n.decommissionStatus ?? "—"}</Td>
                  <Td numeric>{n.workingCopies}</Td>
                  <Td numeric>{n.replicasHeld}</Td>
                  <Td>
                    <RowActions>
                      <NodeActions
                        region={detail.region}
                        node={n.name}
                        verbs={nodeVerbs(n)}
                      />
                    </RowActions>
                  </Td>
                </Tr>
              ))}
            </tbody>
          </DataTable>
        )}
        <p className="border-t border-border px-4 py-2 text-caption text-muted-foreground">
          A drained node is safe to delete only once its status reads <span className="font-mono">drained</span> with a
          timestamp. Running worktrees keep running while a node drains.
        </p>
      </Section>

      <Section
        eyebrow="Workloads"
        title="Region workloads"
        count={detail.workloads.length}
      >
        <RollTable workloads={detail.workloads} onRoll={rollWorkloadAction} />
      </Section>

      <Section
        eyebrow="Configuration"
        title="ClusterSettings/default"
        toolbar={
          <Link href="/superadmin/configuration" className="text-caption text-primary hover:underline">
            All fields
          </Link>
        }
      >
        {Object.keys(detail.settings).length === 0 ? (
          <EmptyState
            action={
              <Link href="/superadmin/configuration" className="text-sm2 text-primary hover:underline">
                Open Configuration
              </Link>
            }
          >
            No ClusterSettings object for this region — every knob is riding env and defaults.
          </EmptyState>
        ) : (
          <>
            <dl className="grid grid-cols-1 gap-x-6 sm:grid-cols-2 lg:grid-cols-3">
              {Object.entries(detail.settings).map(([k, v]) => (
                <div key={k} className="flex items-baseline justify-between gap-2 border-b border-border py-1.5">
                  <dt className="text-caption text-muted-foreground">{k}</dt>
                  <dd className="text-sm2 tabular-nums">{fmt(v)}</dd>
                </div>
              ))}
            </dl>
            <p className="pt-3 text-caption text-muted-foreground">
              Values resolve stored → env → default. Boot-marked fields roll the agent DaemonSet when
              saved; live fields take effect on the next beat.
            </p>
          </>
        )}
      </Section>
    </div>
  );
}
