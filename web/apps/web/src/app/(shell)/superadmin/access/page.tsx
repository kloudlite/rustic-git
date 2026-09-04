import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { PageHeader } from "../page-header";
import { AccessTable } from "./access-table";

export const metadata: Metadata = { title: "Access" };

export default async function AccessPage() {
  const { session, token } = await requireSuperadmin("/superadmin/access");
  const r = await api.listSuperadmins(token);
  if (!r.ok) throw new Error(r.message);

  return (
    <div className="space-y-4">
      <AutoRefresh intervalMs={10_000} />
      <PageHeader title="Access" purpose="Who holds the superadmin claim, and how it was granted." />
      <AccessTable rows={r.value} selfEmail={session.user.email} />
    </div>
  );
}
