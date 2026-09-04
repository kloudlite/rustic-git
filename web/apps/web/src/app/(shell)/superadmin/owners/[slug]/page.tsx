import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { DIMS, dimLabel, dimUnit, requestedDiffs } from "@/lib/quota";
import { limitSource } from "@/lib/owners-sort";
import { eventSummary } from "@/lib/history";
import { when } from "@/lib/time";
import type { Tone } from "@/lib/console";
import { Section } from "../../ui/section";
import { KpiStrip, KpiTile } from "../../ui/kpi";
import { CapacityBar } from "../../ui/capacity-bar";
import { Pill } from "../../ui/pill";
import { DataTable, EmptyState, Td, Th, Tr } from "../../ui/data-table";
import { Timeline, TimelineRow } from "../../ui/timeline";
import { SetQuotaForm } from "./set-quota-form";
import { LiveObjects } from "./live-objects";

export async function generateMetadata({ params }: { params: Promise<{ slug: string }> }): Promise<Metadata> {
  const { slug } = await params;
  return { title: slug };
}

const STATE_TONE: Record<string, Tone> = { pending: "warn", approved: "ok", denied: "critical" };

/** `Owner.dc.html`: header, KPI strip, then Quota / Allocation / Storage / Requests / History as
 *  sections in that order — the operator's read is "how tight, what's live, what's stored, what
 *  are they asking for, what did we do to them". */
export default async function Page({ params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const { token } = await requireSuperadmin(`/superadmin/owners/${slug}`);
  // ponytail: quota requests are the only request kind today; this becomes the generic requests
  // list when that lands, same table.
  const [r, requestsR, eventsR] = await Promise.all([
    api.adminOwnerDetail(slug, token),
    api.adminListQuotaRequests(token, { owner: slug }),
    api.adminHistoryEvents({ owner: slug, limit: 20 }, token),
  ]);
  if (!r.ok) {
    if (r.kind === "notFound") notFound();
    throw new Error(r.message);
  }
  const owner = r.value;
  const requests = requestsR.ok ? requestsR.value : owner.requests;
  const pending = requests.filter((q) => q.state === "pending");
  const detached = owner.volumes.filter((v) => v.deleted);
  const events = eventsR.ok ? eventsR.value.events : [];

  return (
    <div className="space-y-4">
      <AutoRefresh />
      <div className="mb-2 flex items-end justify-between gap-4">
        <div>
          <p className="text-caption text-muted-foreground">
            <Link href="/superadmin/owners" className="hover:underline">Owners</Link> / {owner.owner}
          </p>
          <h1 className="flex items-center gap-2 text-base font-medium">
            {owner.owner}
            <Pill>{owner.isTeam ? "team" : "person"}</Pill>
          </h1>
        </div>
        <div className="flex items-center gap-2">
          <Link
            href={`/${encodeURIComponent(owner.owner)}/workspaces`}
            className="inline-flex h-8 items-center border border-border px-3 text-sm2 font-medium hover:bg-muted"
          >
            Open as {owner.owner}
          </Link>
          <SetQuotaForm owner={owner.owner} limit={owner.limit} />
        </div>
      </div>

      <KpiStrip>
        <KpiTile label="Live workspaces" value={owner.used.workspaces} sub={`of ${owner.limit.workspaces} allowed`} />
        <KpiTile label="Live environments" value={owner.used.environments} sub={`of ${owner.limit.environments} allowed`} />
        <KpiTile
          label="Snapshots"
          value={owner.used.snapshots}
          sub={`of ${owner.limit.snapshots} allowed · ${detached.length} detached`}
        />
        <KpiTile label="Disk" value={`${owner.used.diskGb} GB`} sub={`of ${owner.limit.diskGb} GB allocated`} />
        <KpiTile
          label="Requests pending"
          value={pending.length}
          sub={pending.length === 0 ? "nothing waiting on a decision" : `oldest ${when(new Date(pending[pending.length - 1].createdAt ?? 0).getTime())}`}
        />
      </KpiStrip>

      <Section
        eyebrow="Quota"
        title="Capacity against the effective limits"
        toolbar={
          <span className="text-caption text-muted-foreground">
            Effective quota: {owner.source === "own" ? `Quota/${owner.owner}` : owner.isTeam ? "default-team" : "default-user"}
          </span>
        }
      >
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {DIMS.map((d) => (
            <div key={d} className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <span className="text-sm2 font-medium">{dimLabel(d)}</span>
                {/* The source chip is the point of this grid: v1 showed six bars and never said
                    whether a limit was the owner's own or the fallback. */}
                <Pill>{limitSource(owner, d)}</Pill>
              </div>
              <CapacityBar used={owner.used[d]} limit={owner.limit[d]} unit={dimUnit(d)} />
            </div>
          ))}
        </div>
      </Section>

      <LiveObjects owner={owner.owner} workspaces={owner.workspaces} environments={owner.environments} />

      <Section eyebrow="Storage" title="Volumes and snapshots" count={owner.volumes.length} bare>
        {owner.volumes.length === 0 ? (
          <EmptyState>Nothing pushed yet — a volume appears here on this owner&rsquo;s first push.</EmptyState>
        ) : (
          <DataTable>
            <thead>
              <tr>
                <Th>Volume</Th>
                <Th>Worktree</Th>
                <Th>Kind</Th>
                <Th numeric>Snapshots</Th>
                <Th>Last push</Th>
              </tr>
            </thead>
            <tbody>
              {owner.volumes.map((v) => (
                <Tr key={v.name}>
                  <Td className="font-mono text-caption">{v.volume ?? v.name}</Td>
                  <Td>
                    <span className="font-mono text-caption">{v.display_name}</span>
                    {/* A detached volume has no working copy left: only its snapshots keep it
                        alive, so it is the one row an operator can safely delete. */}
                    {v.deleted && <Pill tone="warn" className="ml-2">detached</Pill>}
                  </Td>
                  <Td className="text-muted-foreground">{v.kind}</Td>
                  <Td numeric>{v.snapshots}</Td>
                  <Td className="text-muted-foreground">
                    {v.last_push_at ? when(new Date(v.last_push_at).getTime()) : "—"}
                  </Td>
                </Tr>
              ))}
            </tbody>
          </DataTable>
        )}
      </Section>

      <Section
        eyebrow="Requests"
        title={`Requests from ${owner.owner}`}
        count={requests.length}
        bare
        toolbar={
          <Link href={`/superadmin/requests?owner=${encodeURIComponent(owner.owner)}`} className="text-caption text-primary hover:underline">
            Queue
          </Link>
        }
      >
        {requests.length === 0 ? (
          <EmptyState>No request from this owner — a quota raise starts on their own dashboard.</EmptyState>
        ) : (
          <DataTable>
            <thead>
              <tr>
                <Th>Kind</Th>
                <Th>Summary</Th>
                <Th>Decision</Th>
                <Th>By</Th>
                <Th>When</Th>
              </tr>
            </thead>
            <tbody>
              {requests.map((q) => (
                <Tr key={q.id}>
                  <Td><Pill>quota</Pill></Td>
                  <Td className="text-muted-foreground">
                    {requestedDiffs(owner.limit, q.requested)
                      .map((d) => `${dimLabel(d.dim).toLowerCase()} ${d.from} → ${d.to}${dimUnit(d.dim) ? ` ${dimUnit(d.dim)}` : ""}`)
                      .join(", ")}
                  </Td>
                  <Td><Pill tone={STATE_TONE[q.state] ?? "neutral"}>{q.state}</Pill></Td>
                  <Td className="text-muted-foreground">{q.decidedBy ?? "—"}</Td>
                  <Td className="text-muted-foreground">{when(new Date(q.createdAt ?? 0).getTime())}</Td>
                </Tr>
              ))}
            </tbody>
          </DataTable>
        )}
      </Section>

      <Section
        eyebrow="History"
        title="Audit trail"
        toolbar={
          <Link href={`/superadmin/audit?target=${encodeURIComponent(owner.owner)}`} className="text-caption text-primary hover:underline">
            All events
          </Link>
        }
      >
        {/* History is optional infrastructure: with no ClickHouse the events list is empty and the
            owner's own audit rows (already on the detail) say the same thing in fewer words. */}
        {events.length > 0 ? (
          <Timeline>
            {events.map((e) => (
              <TimelineRow key={e.id} at={when(new Date(e.ts).getTime())} actor={e.actor} note={e.attrs.note ?? null}>
                {eventSummary(e)}
              </TimelineRow>
            ))}
          </Timeline>
        ) : owner.audit.length > 0 ? (
          <Timeline>
            {owner.audit.map((a, i) => (
              <TimelineRow key={`${a.ts}-${i}`} at={when(new Date(a.ts).getTime())} actor={a.actor} note={a.reason ?? null}>
                {a.actor} {a.action} {a.target} · {a.result}
              </TimelineRow>
            ))}
          </Timeline>
        ) : (
          <EmptyState
            action={
              <Link href="/superadmin/audit" className="text-sm2 text-primary hover:underline">
                Open the audit log
              </Link>
            }
          >
            No recorded write against this owner.
          </EmptyState>
        )}
      </Section>
    </div>
  );
}
