import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import type { AuditFilter } from "@/lib/audit";
import { PageHeader } from "../page-header";
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

  const res = await api.adminAudit(token, filter);
  const page = res.ok ? res.value : { rows: [], next_cursor: null };

  return (
    <div>
      <PageHeader title="Audit" purpose="Who did what, when, and why — every superadmin write, forever." />
      <AuditFilters filter={filter} />
      {!res.ok && (
        <p className="mb-3 border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 text-destructive">
          {res.message}
        </p>
      )}
      <AuditTable initialPage={page} filter={filter} />
    </div>
  );
}
