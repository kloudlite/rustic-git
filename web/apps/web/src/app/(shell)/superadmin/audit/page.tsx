import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import { PageHeader } from "../page-header";

export const metadata: Metadata = { title: "Audit" };

export default async function AuditPage() {
  await requireSuperadmin("/superadmin/audit");
  return (
    <div>
      <PageHeader title="Audit" purpose="Who did what, when, and why — every superadmin write, forever." />
      <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
        Nothing here yet.
      </p>
    </div>
  );
}
