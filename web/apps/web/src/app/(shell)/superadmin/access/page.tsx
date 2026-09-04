import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { PageHeader } from "../page-header";
import { AccessTable } from "./access-table";

export const metadata: Metadata = { title: "Access" };

export default async function AccessPage() {
  const { session, token } = await requireSuperadmin("/superadmin/access");
  const r = await api.listSuperadmins(token);
  if (!r.ok) throw new Error(r.message);

  return (
    <div>
      <PageHeader title="Access" purpose="Who can act as superadmin. Add by email; removal needs a second confirmation." />
      <AccessTable rows={r.value} selfEmail={session.user.email} />
    </div>
  );
}
