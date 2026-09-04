import type { Metadata } from "next";
import Link from "next/link";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { when } from "@/lib/time";
import { DIMS, dimLabel } from "@/lib/quota";
import { attentionTone, deltaLabel } from "@/lib/history";
import { PageHeader } from "./page-header";
import { regionCapacity } from "./overview";
import { Section } from "./ui/section";
import { KpiStrip, KpiTile } from "./ui/kpi";
import { CapacityBar } from "./ui/capacity-bar";
import { Pill } from "./ui/pill";
import { DataTable, Th, Td, Tr, EmptyState } from "./ui/data-table";
import { Timeline, TimelineRow } from "./ui/timeline";

export const metadata: Metadata = { title: "Overview" };

const HHMM = new Intl.DateTimeFormat("en", { hour: "2-digit", minute: "2-digit", hour12: false, timeZone: "UTC" });

/** The landing screen (`Main.dc.html`): five KPI tiles with 7-day sparklines, a needs-attention
 *  feed beside capacity per region, then recent activity and the waiting queue.
 *
 *  One `Promise.all`, not a waterfall: eight reads at ~13 ms each in-cluster is one render, and
 *  serialising them would put the 10 s poll behind its own previous run. Every history read
 *  degrades on its own (`adminSeries` never rejects), so a missing ClickHouse costs sparklines,
 *  not the page.
 *
 *  // ponytail: the "requests waiting" section reads the existing `QuotaRequestDoc` queue
 *  // (`api.adminListQuotaRequests`), not the generic `RequestDoc` sub-project B introduces —
 *  // that project hasn't landed in this tree yet. Swap to `adminListRequests`/`kindLabel` once
 *  // it does; the section's shape (kind pill, owner, summary, age) does not need to change. */
export default async function OverviewPage() {
  const { token } = await requireSuperadmin("/superadmin");
  const opts = { range: "7d", step: "1d" };
  const [o, clusters, pending, pendingS, firingS, over80S, wsS, envS] = await Promise.all([
    api.adminOverview(token),
    api.adminClusters(token),
    api.adminListQuotaRequests(token, { state: "pending" }),
    api.adminSeries("pending_requests", opts, token),
    api.adminSeries("firing_signals", opts, token),
    api.adminSeries("owners_over_80", opts, token),
    api.adminSeries("live_workspaces", opts, token),
    api.adminSeries("live_environments", opts, token),
  ]);
  if (!o.ok) throw new Error(o.message);
  const ov = o.value;
  const regions = clusters.ok ? clusters.value : [];
  const queue = pending.ok ? pending.value : [];

  // Per-region gauges are three more reads each; regions are two, so this is bounded and still
  // one round of parallelism rather than a nested waterfall.
  const capacity = await Promise.all(
    regions.map(async (r) => ({
      region: r,
      gauges: regionCapacity(
        await api.adminSeries("pool_used", { ...opts, region: r.region }, token),
        await api.adminSeries("cpu_used", { ...opts, region: r.region }, token),
        await api.adminSeries("memory_used", { ...opts, region: r.region }, token),
      ),
    })),
  );

  return (
    <div className="space-y-4">
      <AutoRefresh />
      <PageHeader title="Overview" purpose="What needs a superadmin across every region, right now." />

      {ov.errors && ov.errors.length > 0 && (
        <p className="border border-border bg-muted/50 px-3 py-2 text-sm2 text-muted-foreground">{ov.errors.join(" · ")}</p>
      )}

      <KpiStrip>
        <KpiTile label="Pending requests" value={queue.length} sub={deltaLabel(pendingS, "requests")} series={pendingS} />
        <KpiTile label="Firing signals" value={firingS.summary.last} sub={deltaLabel(firingS, "signals")} series={firingS} />
        <KpiTile label="Owners over 80%" value={over80S.summary.last} sub={deltaLabel(over80S, "owners")} series={over80S} />
        <KpiTile label="Live workspaces" value={ov.fleet.workspaces} sub={deltaLabel(wsS, "workspaces")} series={wsS} />
        <KpiTile label="Live environments" value={ov.fleet.environments} sub={deltaLabel(envS, "environments")} series={envS} />
      </KpiStrip>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1fr)_420px]">
        <Section eyebrow="Operations" title="Needs attention" count={ov.attention.length} bare>
          {ov.attention.length === 0 ? (
            <EmptyState action={<Link className="text-sm2 text-primary underline-offset-4 hover:underline" href="/superadmin/monitoring">Open monitoring</Link>}>
              Nothing needs a superadmin right now.
            </EmptyState>
          ) : (
            <ul>
              {ov.attention.map((a, i) => (
                <li key={`${a.kind}-${i}`} className="group/row flex items-center gap-3 border-b border-border px-4 py-2 last:border-0 hover:bg-muted">
                  <Pill tone={attentionTone(a.kind)}>{a.kind}</Pill>
                  <p className="min-w-0 flex-1 truncate text-sm2">{a.detail}</p>
                  <Link href={a.href} className="text-sm2 text-muted-foreground group-hover/row:text-primary">Open</Link>
                </li>
              ))}
            </ul>
          )}
        </Section>

        <Section eyebrow="Fleet" title="Capacity by region" count={regions.length}>
          <div className="flex flex-col gap-4">
            {capacity.map(({ region, gauges }) => (
              <div key={region.region} className="flex flex-col gap-2">
                <div className="flex items-center gap-2">
                  <span className="text-sm2 font-medium">{region.region}</span>
                  <Pill tone={region.nodesReady === region.nodesTotal ? "ok" : "warn"}>
                    {region.nodesReady} of {region.nodesTotal} nodes ready
                  </Pill>
                </div>
                <CapacityBar used={gauges.pool.used} limit={gauges.pool.limit} unit="%" label="Disk pool" />
                <CapacityBar used={gauges.cpu.used} limit={gauges.cpu.limit} unit="%" label="CPU" />
                <CapacityBar used={gauges.memory.used} limit={gauges.memory.limit} unit="%" label="Memory" />
                <Link href={`/superadmin/clusters/${encodeURIComponent(region.region)}`} className="text-caption text-primary underline-offset-4 hover:underline">
                  Open
                </Link>
              </div>
            ))}
            {regions.length === 0 && <EmptyState>No regions yet.</EmptyState>}
          </div>
        </Section>
      </div>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1fr)_420px]">
        <Section
          eyebrow="History"
          title="Recent activity"
          toolbar={<Link href="/superadmin/audit" className="text-caption text-primary underline-offset-4 hover:underline">Audit log</Link>}
        >
          {ov.recentAudit.length === 0 ? (
            <EmptyState action={<Link className="text-sm2 text-primary underline-offset-4 hover:underline" href="/superadmin/audit">Open the audit log</Link>}>
              No activity has been recorded yet.
            </EmptyState>
          ) : (
            <Timeline>
              {ov.recentAudit.map((e, i) => (
                <TimelineRow key={i} at={HHMM.format(new Date(e.ts))} actor={e.actor} note={null}>
                  {`${e.actor} ${e.action} ${e.target}`}
                </TimelineRow>
              ))}
            </Timeline>
          )}
        </Section>

        <Section
          eyebrow="Queue"
          title="Requests waiting"
          count={queue.length}
          bare
          toolbar={<Link href="/superadmin/requests" className="text-caption text-primary underline-offset-4 hover:underline">Open queue</Link>}
        >
          {queue.length === 0 ? (
            <EmptyState>No owner is waiting on a decision.</EmptyState>
          ) : (
            <DataTable>
              <thead>
                <tr><Th>Owner</Th><Th>Summary</Th><Th numeric>Age</Th></tr>
              </thead>
              <tbody>
                {queue.slice(0, 5).map((r) => (
                  <Tr key={r.id}>
                    <Td>{r.owner}</Td>
                    <Td className="max-w-0 truncate">
                      {DIMS.filter((d) => r.requested[d] !== undefined)
                        .map((d) => `${dimLabel(d)} → ${r.requested[d]}`)
                        .join(", ")}
                    </Td>
                    <Td numeric>{when(new Date(r.createdAt ?? 0).getTime())}</Td>
                  </Tr>
                ))}
              </tbody>
            </DataTable>
          )}
        </Section>
      </div>
    </div>
  );
}
