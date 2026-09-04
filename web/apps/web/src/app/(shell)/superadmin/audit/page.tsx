import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import type { AuditFilter } from "@/lib/audit";
import { isRefusal, resultPill } from "@/lib/audit-result";
import { deltaLabel } from "@/lib/history";
import { when } from "@/lib/time";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { PageHeader } from "../page-header";
import { KpiStrip, KpiTile } from "../ui/kpi";
import { AuditTable } from "./audit-table";
import { AuditFilters } from "./audit-filters";

export const metadata: Metadata = { title: "Audit" };

// Filtering is a plain GET form: the browser navigates with the fields as query params, this
// server component re-runs with them, and there is no client JS needed just to narrow a filter —
// only "Load more" (accumulating pages) and CSV export need a client component at all.
export default async function AuditPage({ searchParams }: { searchParams: Promise<Record<string, string | undefined>> }) {
  const { token } = await requireSuperadmin("/superadmin/audit");
  const sp = await searchParams;
  const filter: AuditFilter = {};
  for (const key of ["actor", "action", "target", "from", "to"] as const) {
    const v = sp[key];
    if (v) filter[key] = v;
  }

  const [res, eventsS] = await Promise.all([
    api.adminAudit(token, filter),
    api.adminSeries("audit_events", { range: "7d", step: "1d" }, token),
  ]);
  const page = res.ok ? res.value : { rows: [], next_cursor: null };
  // Distinct action words from the rows already fetched — the datalist's suggestions can never
  // drift from the real backend vocabulary because it never names one itself.
  const knownActions = [...new Set(page.rows.map((r) => r.action))].sort();
  // Every tile is computed from the page already on screen, never a second query: a KPI that
  // disagrees with the table under it is worse than one that only counts what is shown.
  const actors = new Set(page.rows.map((r) => r.actor));
  const refusals = page.rows.filter(isRefusal);
  const exports = page.rows.filter((r) => r.action === "audit.export");

  return (
    <div className="space-y-4">
      <AutoRefresh intervalMs={10_000} />
      <PageHeader title="Audit" purpose="Every superadmin action and every refusal, with the note that justified it." />
      <KpiStrip cols={4}>
        <KpiTile label="Events today" value={eventsS.available ? eventsS.summary.last : "—"} sub={deltaLabel(eventsS)} series={eventsS} />
        <KpiTile label="Actors" value={actors.size} sub={[...actors].slice(0, 4).join(", ") || "nobody yet"} />
        <KpiTile
          label="Refusals"
          value={refusals.length}
          sub={
            refusals.length
              ? `${refusals.filter((r) => resultPill(r).tone === "warn").length} of them 409`
              : "no refusal in view"
          }
        />
        <KpiTile
          label="Exports"
          value={exports.length}
          sub={exports[0] ? `${exports[0].actor}, ${when(new Date(exports[0].ts).getTime())}` : "no export in view"}
        />
      </KpiStrip>
      {!res.ok && (
        <p className="border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 text-destructive">
          {res.message}
        </p>
      )}
      <AuditTable
        initialPage={page}
        filter={filter}
        filters={<AuditFilters filter={filter} knownActions={knownActions} />}
      />
    </div>
  );
}
