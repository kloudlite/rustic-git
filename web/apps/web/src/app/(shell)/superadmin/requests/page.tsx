import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { PageHeader } from "../page-header";
import { RequestQueue } from "./request-queue";

export const metadata: Metadata = { title: "Requests" };

export default async function Page({
  searchParams,
}: {
  searchParams: Promise<{ owner?: string; state?: string }>;
}) {
  const { token } = await requireSuperadmin("/superadmin/requests");
  const { owner, state } = await searchParams;
  // One fetch of the queue: it feeds the Pending/Decided tabs, the free-text/dimension/age
  // filters, and a row's own owner-history in the decision panel — all client-side, since the
  // fleet-wide queue is small. `?owner=`/`?state=` narrow it SERVER-side so an owner-detail link
  // lands on that owner's requests rather than the whole fleet's.
  const [reqs, owners] = await Promise.all([
    api.adminListQuotaRequests(token, { owner, state }),
    api.adminOwners(token),
  ]);
  const rows = reqs.ok ? reqs.value : [];
  // `adminOwners` carries the limit and in-use count each request's diff is read against; a
  // failed read leaves the panel showing zeros rather than taking the queue down with it.
  const usageRows = owners.ok ? owners.value : [];

  return (
    <div>
      <PageHeader title="Requests" purpose="Quota raise requests waiting on a decision." />
      <RequestQueue rows={rows} usage={usageRows} />
    </div>
  );
}
