import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { PageHeader } from "../page-header";
import { RequestQueue } from "./request-queue";

export const metadata: Metadata = { title: "Requests" };

export default async function Page() {
  const { token } = await requireSuperadmin("/superadmin/requests");
  // One fetch of the whole queue: it feeds the Pending/Decided tabs, the free-text/dimension/age
  // filters, and a row's own owner-history in the decision panel — all client-side, since the
  // fleet-wide queue is small. `adminUsage` gives the current limit each request's diff is against.
  const [reqs, usage] = await Promise.all([api.adminListQuotaRequests(token), api.adminUsage(token)]);
  const rows = reqs.ok ? reqs.value : [];
  const usageRows = usage.ok ? usage.value : [];

  return (
    <div>
      <PageHeader title="Requests" purpose="Quota raise requests waiting on a decision." />
      <RequestQueue rows={rows} usage={usageRows} />
    </div>
  );
}
