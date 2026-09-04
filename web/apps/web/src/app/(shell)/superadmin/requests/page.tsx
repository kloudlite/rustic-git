import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { when } from "@/lib/time";
import { PageHeader } from "../page-header";
import { KpiStrip, KpiTile } from "../ui/kpi";
import { RequestQueue } from "./request-queue";

export const metadata: Metadata = { title: "Requests" };

/** The generic queue (`Requests.dc.html`): quota, access, region and other in one table.
 *
 *  The tabs and the three filters live in the URL, not in component state: an operator sends
 *  "the oldest open access requests" to a colleague as a link, and the 10 s poll re-runs this
 *  component — state held in the client would survive that, but the shared link would not.
 *
 *  `owner` and `kind` narrow SERVER-side (an owner-detail link lands on that owner's requests);
 *  the finer filters stay client-side over the one fetched page. */
export default async function Page({
  searchParams,
}: {
  searchParams: Promise<{ owner?: string; state?: string; kind?: string; ownerType?: string; age?: string }>;
}) {
  const { token } = await requireSuperadmin("/superadmin/requests");
  const sp = await searchParams;
  const opts = { range: "7d", step: "1d" };
  const [reqs, owners, decidedS, p50S] = await Promise.all([
    // Fleet-wide on purpose: the KPI strip counts the whole queue, and `filterQueue` narrows by
    // kind client-side. Only `owner` narrows server-side, so an owner-detail link lands scoped.
    api.adminListRequests(token, { owner: sp.owner }),
    api.adminOwners(token),
    api.adminSeries("decided_requests", opts, token),
    api.adminSeries("time_to_decide_p50", opts, token),
  ]);
  // A failed owners read costs the usage line and the owner-type filter, never the queue itself.
  const rows = reqs.ok ? reqs.value : [];
  const usage = owners.ok ? owners.value : [];
  const open = rows.filter((r) => r.state === "pending");
  const oldest = [...open].sort((a, b) => new Date(a.createdAt ?? 0).getTime() - new Date(b.createdAt ?? 0).getTime())[0];
  const byKind = ["quota", "access", "region", "other"]
    .map((k) => ({ k, n: open.filter((r) => r.kind === k).length }))
    .filter((x) => x.n > 0)
    .map((x) => `${x.n} ${x.k}`)
    .join(" · ");

  return (
    <div className="space-y-4">
      <AutoRefresh />
      <PageHeader
        title="Requests"
        purpose="Every kind of request an owner can raise: quota, access, region, or anything else."
      />
      <KpiStrip cols={4}>
        <KpiTile label="Open" value={open.length} sub={byKind || "nothing open"} />
        <KpiTile
          label="Oldest open"
          value={oldest ? when(new Date(oldest.createdAt ?? 0).getTime()) : "—"}
          sub={oldest ? `${oldest.owner} · ${oldest.kind}` : "nothing waiting"}
        />
        <KpiTile
          label="Decided this week"
          value={decidedS.available ? decidedS.summary.last : "—"}
          sub={decidedS.available ? `${decidedS.summary.min}–${decidedS.summary.max} per day` : "history unavailable"}
          series={decidedS}
        />
        <KpiTile
          label="Median time to decide"
          value={p50S.available ? `${p50S.summary.last} h` : "—"}
          sub={
            p50S.available
              ? p50S.summary.delta === 0
                ? `unchanged from ${p50S.summary.last} h last week`
                : `${p50S.summary.delta > 0 ? "up" : "down"} from ${p50S.summary.last - p50S.summary.delta} h last week`
              : "history unavailable"
          }
          series={p50S}
        />
      </KpiStrip>
      <RequestQueue rows={rows} usage={usage} />
    </div>
  );
}
