import type { Metadata } from "next";
import Link from "next/link";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { when } from "@/lib/time";
import { deltaLabel, eventSummary } from "@/lib/history";
import { kindLabel } from "@/lib/requests";
import { summaryLine } from "@/lib/request-queue";
import { PageHeader } from "./page-header";
import { regionCapacity } from "./overview";
import { AttentionFeed } from "./attention-feed";
import { RegionCapacityCard } from "./overview-capacity";
import { Section } from "./ui/section";
import { KpiStrip, KpiTile } from "./ui/kpi";
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
 *  not the page. "Requests waiting" reads the generic queue rather than `/admin/overview`'s
 *  quota-only one: the two disagree the moment an owner asks for anything else, and the tile links
 *  straight into the queue this row came from. */
export default async function OverviewPage() {
  const { token } = await requireSuperadmin("/superadmin");
  const opts = { range: "7d", step: "1d" };
  const [o, clusters, events, queued, pendingS, firingS, over80S, wsS, envS] = await Promise.all([
    api.adminOverview(token),
    api.adminClusters(token),
    api.adminHistoryEvents({ limit: 5 }, token),
    api.adminListRequests(token, { state: "pending" }),
    api.adminSeries("pending_requests", opts, token),
    api.adminSeries("firing_signals", opts, token),
    api.adminSeries("owners_over_80", opts, token),
    api.adminSeries("live_workspaces", opts, token),
    api.adminSeries("live_environments", opts, token),
  ]);
  if (!o.ok) throw new Error(o.message);
  const ov = o.value;
  const regions = clusters.ok ? clusters.value : [];
  const queue = queued.ok ? queued.value : [];

  // Three more series per region. Regions are a handful, so this is one more round of parallelism
  // rather than a nested waterfall — awaiting them one at a time would cost 3n round trips.
  const capacity = await Promise.all(
    regions.map(async (region) => {
      const [pool, cpu, memory] = await Promise.all([
        api.adminSeries("pool_used", { ...opts, region: region.region }, token),
        api.adminSeries("cpu_used", { ...opts, region: region.region }, token),
        api.adminSeries("memory_used", { ...opts, region: region.region }, token),
      ]);
      return { region, gauges: regionCapacity(pool, cpu, memory) };
    }),
  );

  // History down is not an error here: the overview's own `recentAudit` carries the same writes,
  // just without the history layer's phrasing, so the section stays populated either way.
  const activity =
    events.ok && events.value.events.length > 0
      ? events.value.events.map((e) => ({ key: e.id, ts: e.ts, actor: e.actor, text: eventSummary(e), note: e.attrs.note ?? null }))
      : ov.recentAudit.map((e, i) => ({
          key: `${e.ts}-${i}`,
          ts: e.ts,
          actor: e.actor,
          text: `${e.actor} ${e.action} ${e.target}`,
          note: e.reason ?? null,
        }));

  const fleetLine = `${ov.fleet.workspaces} workspaces, ${ov.fleet.environments} environments and ${ov.fleet.owners} owners across ${regions.length} regions.`;

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
          <AttentionFeed items={ov.attention} fleetLine={fleetLine} />
        </Section>

        <Section
          eyebrow="Fleet"
          title="Capacity by region"
          count={regions.length}
          toolbar={
            <Link href="/superadmin/clusters" className="text-caption text-primary underline-offset-4 hover:underline">
              All clusters
            </Link>
          }
        >
          {capacity.length === 0 ? (
            <EmptyState
              action={
                <Link className="text-sm2 text-primary underline-offset-4 hover:underline" href="/superadmin/clusters">
                  Add a region
                </Link>
              }
            >
              No region is registered yet.
            </EmptyState>
          ) : (
            <div className="flex flex-col gap-4">
              {capacity.map(({ region, gauges }) => (
                <RegionCapacityCard key={region.region} region={region} gauges={gauges} />
              ))}
            </div>
          )}
        </Section>
      </div>

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-[minmax(0,1fr)_420px]">
        <Section
          eyebrow="History"
          title="Recent activity"
          toolbar={
            <Link href="/superadmin/audit" className="text-caption text-primary underline-offset-4 hover:underline">
              Audit log
            </Link>
          }
        >
          {activity.length === 0 ? (
            <EmptyState
              action={
                <Link className="text-sm2 text-primary underline-offset-4 hover:underline" href="/superadmin/audit">
                  Open the audit log
                </Link>
              }
            >
              No activity has been recorded yet.
            </EmptyState>
          ) : (
            <Timeline>
              {activity.map((a) => (
                <TimelineRow key={a.key} at={HHMM.format(new Date(a.ts))} actor={a.actor} note={a.note}>
                  {a.text}
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
          toolbar={
            <Link href="/superadmin/requests" className="text-caption text-primary underline-offset-4 hover:underline">
              Open queue
            </Link>
          }
        >
          {queue.length === 0 ? (
            <EmptyState>No owner is waiting on a decision.</EmptyState>
          ) : (
            <DataTable>
              <thead>
                <tr>
                  <Th>Kind</Th>
                  <Th>Owner</Th>
                  <Th>Summary</Th>
                  <Th numeric>Age</Th>
                </tr>
              </thead>
              <tbody>
                {queue.slice(0, 5).map((r) => (
                  <Tr key={r.id}>
                    <Td>
                      <Pill tone="info">{kindLabel(r.kind)}</Pill>
                    </Td>
                    <Td>{r.owner}</Td>
                    <Td className="max-w-0 truncate">{summaryLine(r)}</Td>
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
