import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { DIMS, type QuotaDim } from "@/lib/quota";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { PageHeader } from "../page-header";
import { KpiStrip, KpiTile } from "../ui/kpi";
import { DefaultsTable } from "./defaults-table";
import { OwnersTable } from "./owners-table";

export const metadata: Metadata = { title: "Owners" };

/** `Owners.dc.html`: KPI strip, the two defaults as ONE comparison table (v1 had two separate
 *  lists that could not be read against each other), then the owners table sorted by pressure. */
export default async function Page() {
  const { token } = await requireSuperadmin("/superadmin/owners");
  // `getQuota` on the caller's own `/v1/quota` route rather than a new admin one: a superadmin
  // claim already passes `may_act_on` for any owner (`scope::may_act_on`), and `default-user`/
  // `default-team` are ordinary owner names to that route.
  // ponytail: the pending count reads the quota-request queue, the only request kind that exists
  // today; swap to the generic requests list when that lands, same shape.
  const [owners, personDefault, teamDefault, pendingR, over80] = await Promise.all([
    api.adminOwners(token),
    api.getQuota("default-user", token),
    api.getQuota("default-team", token),
    api.adminListQuotaRequests(token, { state: "pending" }),
    api.adminSeries("owners_over_80", { range: "7d", step: "1d" }, token),
  ]);
  if (!owners.ok) throw new Error(owners.message);
  const rows = owners.value;
  const pending = pendingR.ok ? pendingR.value : [];
  const teams = rows.filter((o) => o.isTeam).length;
  const atLimit = rows.filter((r) => DIMS.some((d) => r.limit[d] > 0 && r.used[d] >= r.limit[d])).length;
  const disk = rows.reduce((n, o) => n + (o.used.diskGb ?? 0), 0);

  // The upgrade path the spec calls for: per dimension, the single highest USED value any owner
  // in the fleet has today — so lowering a default below what someone already relies on is
  // visible in the same form, without a second endpoint.
  const fleetMax = DIMS.reduce(
    (acc, d) => {
      acc[d] = rows.length > 0 ? Math.max(...rows.map((r) => r.used[d])) : 0;
      return acc;
    },
    {} as Record<QuotaDim, number>,
  );

  return (
    <div className="space-y-4">
      <AutoRefresh />
      <PageHeader title="Owners" purpose="Every person and team that can allocate, and how close each one is to a wall." />
      <KpiStrip>
        <KpiTile label="Owners" value={rows.length} sub={`${rows.length - teams} people · ${teams} teams`} />
        <KpiTile label="Teams" value={teams} sub="of every owner that can allocate" />
        <KpiTile
          label="Over 80% of a limit"
          value={over80.summary.last}
          sub={over80.available ? `${atLimit} of them at the limit` : "history unavailable"}
          series={over80}
        />
        <KpiTile
          label="Pending requests"
          value={pending.length}
          sub={`from ${new Set(pending.map((p) => p.owner)).size} owners`}
        />
        <KpiTile label="Disk allocated" value={`${disk} GB`} sub="across every pool" />
      </KpiStrip>
      <DefaultsTable
        personDefault={personDefault.ok ? personDefault.value : null}
        teamDefault={teamDefault.ok ? teamDefault.value : null}
        fleetMax={fleetMax}
      />
      <OwnersTable rows={rows} pending={pending} />
    </div>
  );
}
