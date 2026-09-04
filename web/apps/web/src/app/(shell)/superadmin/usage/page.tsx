import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { QuotaBar } from "@/components/app/quota-bar";
import { PageHeader } from "../page-header";

export const metadata: Metadata = { title: "Usage" };

export default async function Page() {
  const { token } = await requireSuperadmin("/superadmin/usage");
  const r = await api.adminUsage(token);
  const rows = r.ok ? r.value : [];

  return (
    <div>
      <PageHeader title="Usage" purpose="Every owner's usage against their quota." />
      <div className="space-y-6">
      {/* ponytail: the owner list is derived from who has ever had a Quota or a request; an owner
          who has neither is not shown. A `GET /admin/quota` (no owner) listing every Quota plus
          every distinct owner label is the upgrade when the list has to be complete. */}
      {rows.length === 0 ? (
        <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
          No owner has a quota or a request yet.
        </p>
      ) : (
        rows.map((row) => (
          <div key={row.owner} className="border border-border bg-card p-4">
            <p className="mb-3 text-sm2 font-medium">{row.owner}</p>
            <QuotaBar report={row} />
          </div>
        ))
      )}
      </div>
    </div>
  );
}
