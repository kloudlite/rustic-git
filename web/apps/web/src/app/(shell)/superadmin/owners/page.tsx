import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { DIMS, type QuotaDim } from "@/lib/quota";
import { PageHeader } from "../page-header";
import { DefaultsCard } from "./defaults-card";
import { OwnersTable } from "./owners-table";

export const metadata: Metadata = { title: "Owners" };

export default async function Page() {
  const { token } = await requireSuperadmin("/superadmin/owners");
  // `getQuota` on the caller's own `/v1/quota` route rather than a new admin one: a superadmin
  // claim already passes `may_act_on` for any owner (`scope::may_act_on`), and `default-user`/
  // `default-team` are ordinary owner names to that route.
  const [owners, personDefault, teamDefault] = await Promise.all([
    api.adminOwners(token),
    api.getQuota("default-user", token),
    api.getQuota("default-team", token),
  ]);
  const rows = owners.ok ? owners.value : [];

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
    <div>
      <PageHeader title="Owners" purpose="Every person and team, with what they use against their limits. Open one to see its objects and set its quota." />
      <div className="flex flex-col gap-6">
        <DefaultsCard
          personDefault={personDefault.ok ? personDefault.value : null}
          teamDefault={teamDefault.ok ? teamDefault.value : null}
          fleetMax={fleetMax}
        />
        {rows.length === 0 ? (
          <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
            No owner has a quota, a request, or a live object yet.
          </p>
        ) : (
          <OwnersTable rows={rows} />
        )}
      </div>
    </div>
  );
}
