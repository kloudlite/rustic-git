import type { Metadata } from "next";
import Link from "next/link";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { when } from "@/lib/time";
import { DIMS, dimLabel } from "@/lib/quota";
import { PageHeader } from "./page-header";
import { needsNothing } from "./overview";

export const metadata: Metadata = { title: "Overview" };

/** Landing view: what needs a decision (pending requests, attention), what just happened (recent
 *  audit), and the fleet's size — one round trip (`GET /admin/overview`), polled like every other
 *  admin page. `errors` on the response names a sub-source that degraded (e.g. the signals scrape
 *  needs `aks`) rather than failing the whole page — rendered as a muted notice, not a throw. */
export default async function OverviewPage() {
  const { token } = await requireSuperadmin("/superadmin");
  const r = await api.adminOverview(token);
  if (!r.ok) throw new Error(r.message);
  const o = r.value;
  const empty = needsNothing(o);

  return (
    <div className="space-y-6">
      <AutoRefresh />
      <PageHeader title="Overview" purpose="What needs attention across every owner, cluster, and request." />

      {o.errors && o.errors.length > 0 && (
        <p className="border border-border bg-muted/50 px-3 py-2 text-sm2 text-muted-foreground">
          {o.errors.join(" · ")}
        </p>
      )}

      {empty ? (
        <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
          Nothing needs attention.
        </p>
      ) : (
        <div className="grid grid-cols-1 gap-4 md:grid-cols-3">
          <div className="flex flex-col gap-3 border border-border bg-card p-4">
            <div className="flex items-center justify-between gap-3">
              <div className="text-sm2 font-medium">Pending requests</div>
              <Link href="/superadmin/requests" className="text-caption text-primary">
                Open queue
              </Link>
            </div>
            <div className="text-xl font-medium tabular-nums">{o.pendingRequests.length}</div>
            {o.pendingRequests.length === 0 ? (
              <p className="text-sm2 text-muted-foreground">None pending.</p>
            ) : (
              <ul className="flex flex-col gap-2 text-sm2">
                {o.pendingRequests.map((req) => (
                  <li key={req.id} className="flex items-center justify-between gap-2">
                    <span className="min-w-0 flex-1 truncate">
                      <span className="font-medium">{req.owner}</span>{" "}
                      <span className="text-muted-foreground">
                        {DIMS.filter((d) => req.requested[d] !== undefined)
                          .map((d) => `${dimLabel(d)} → ${req.requested[d]}`)
                          .join(", ")}
                      </span>
                    </span>
                    <span className="shrink-0 text-caption text-muted-foreground">{when(new Date(req.createdAt ?? 0).getTime())}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>

          <div className="flex flex-col gap-3 border border-border bg-card p-4">
            <div className="flex items-center justify-between gap-3">
              <div className="text-sm2 font-medium">Attention</div>
              <Link href="/superadmin/monitoring" className="text-caption text-primary">
                Monitoring
              </Link>
            </div>
            <div className="text-xl font-medium tabular-nums">{o.attention.length}</div>
            {o.attention.length === 0 ? (
              <p className="text-sm2 text-muted-foreground">Nothing firing.</p>
            ) : (
              <ul className="flex flex-col gap-2 text-sm2">
                {o.attention.map((a, i) => (
                  <li key={i}>
                    <Link href={a.href} className="text-foreground hover:text-primary">
                      {a.detail}
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </div>

          <div className="flex flex-col gap-3 border border-border bg-card p-4">
            <div className="flex items-center justify-between gap-3">
              <div className="text-sm2 font-medium">Recent activity</div>
              <Link href="/superadmin/audit" className="text-caption text-primary">
                Audit
              </Link>
            </div>
            {o.recentAudit.length === 0 ? (
              <p className="text-sm2 text-muted-foreground">No writes yet.</p>
            ) : (
              <ul className="flex flex-col gap-2 text-sm2">
                {o.recentAudit.map((e, i) => (
                  <li key={i} className="truncate">
                    <span className="text-caption text-muted-foreground">{when(new Date(e.ts).getTime())}</span>{" "}
                    {e.actor} {e.action} {e.target}
                  </li>
                ))}
              </ul>
            )}
          </div>
        </div>
      )}

      <div className="flex flex-col gap-3 border border-border bg-card p-4">
        <div className="text-sm2 font-medium">Fleet</div>
        <div className="overflow-x-auto">
          <table className="w-full text-sm2">
            <thead>
              <tr className="border-b border-border text-caption text-muted-foreground">
                <th className="py-1.5 pr-3 text-left font-medium">Region</th>
                <th className="py-1.5 pr-3 text-left font-medium">Owners</th>
                <th className="py-1.5 pr-3 text-left font-medium">Workspaces</th>
                <th className="py-1.5 pr-3 text-left font-medium">Environments</th>
                <th className="py-1.5 pr-3 text-left font-medium">Snapshots</th>
                <th className="py-1.5 pr-3 text-left font-medium">Disk allocated</th>
              </tr>
            </thead>
            <tbody>
              {Object.entries(o.fleet.perRegion).map(([region, f]) => (
                <tr key={region} className="border-b border-border last:border-0">
                  <td className="py-1.5 pr-3 font-mono">{region}</td>
                  <td className="py-1.5 pr-3 tabular-nums">{f.owners}</td>
                  <td className="py-1.5 pr-3 tabular-nums">{f.workspaces}</td>
                  <td className="py-1.5 pr-3 tabular-nums">{f.environments}</td>
                  <td className="py-1.5 pr-3 tabular-nums">{f.snapshots}</td>
                  <td className="py-1.5 pr-3 tabular-nums">{f.diskGb} GB</td>
                </tr>
              ))}
              {Object.keys(o.fleet.perRegion).length === 0 && (
                <tr>
                  <td colSpan={6} className="py-4 text-center text-muted-foreground">
                    No regions yet.
                  </td>
                </tr>
              )}
            </tbody>
            <tfoot>
              <tr className="text-caption text-muted-foreground">
                <td className="pt-2 pr-3">total</td>
                <td className="pt-2 pr-3 tabular-nums">{o.fleet.owners}</td>
                <td className="pt-2 pr-3 tabular-nums">{o.fleet.workspaces}</td>
                <td className="pt-2 pr-3 tabular-nums">{o.fleet.environments}</td>
                <td className="pt-2 pr-3 tabular-nums">{o.fleet.snapshots}</td>
                <td className="pt-2 pr-3 tabular-nums">{o.fleet.diskGbTotal} GB</td>
              </tr>
            </tfoot>
          </table>
        </div>
      </div>
    </div>
  );
}
