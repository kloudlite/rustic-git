import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { Badge } from "@/components/ui/badge";
import { QuotaBar } from "@/components/app/quota-bar";
import { dimLabel, type QuotaDim } from "@/lib/quota";
import { when } from "@/lib/time";
import { SetQuotaForm } from "./set-quota-form";
import { LiveObjects } from "./live-objects";

export async function generateMetadata({ params }: { params: Promise<{ slug: string }> }): Promise<Metadata> {
  const { slug } = await params;
  return { title: slug };
}

const REQUEST_STATE_TONE: Record<string, string> = {
  pending: "border-warning/40 bg-warning/10 text-warning",
  approved: "border-success/40 bg-success/10 text-success",
  denied: "border-destructive/40 bg-destructive/10 text-destructive",
};

export default async function Page({ params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const { token } = await requireSuperadmin(`/superadmin/owners/${slug}`);
  const r = await api.adminOwnerDetail(slug, token);
  if (!r.ok) {
    if (r.kind === "notFound") notFound();
    throw new Error(r.message);
  }
  const owner = r.value;
  const detached = owner.volumes.filter((v) => v.deleted);

  return (
    <div>
      <div className="mb-6 flex items-end justify-between gap-4">
        <div>
          <h1 className="flex items-center gap-2 text-base font-medium">
            {owner.owner}
            <Badge variant="outline">{owner.isTeam ? "team" : "person"}</Badge>
          </h1>
          <p className="text-sm2 text-muted-foreground">Live objects, quota, and history for this owner.</p>
        </div>
        <div className="flex gap-2">
          <SetQuotaForm owner={owner.owner} limit={owner.limit} />
          <Link
            href={`/${encodeURIComponent(owner.owner)}/workspaces`}
            className="inline-flex h-8 items-center border border-border px-3 text-sm2 font-medium hover:bg-muted"
          >
            Open as {owner.owner}
          </Link>
        </div>
      </div>

      <div className="flex flex-col gap-6">
        <div className="grid grid-cols-1 gap-4 border border-border bg-card p-4 sm:grid-cols-2 lg:grid-cols-3">
          <QuotaBar report={owner} source={owner.source === "own" ? "own quota" : owner.isTeam ? "team default" : "person default"} />
        </div>

        <div className="grid grid-cols-1 gap-6 lg:grid-cols-[3fr_2fr]">
          <LiveObjects owner={owner.owner} workspaces={owner.workspaces} environments={owner.environments} />

          <div className="flex flex-col gap-6">
            <div className="border border-border bg-card p-4">
              <div className="mb-3 flex items-center justify-between">
                <span className="text-sm2 font-medium">Requests</span>
                <Link href={`/superadmin/requests?owner=${encodeURIComponent(owner.owner)}`} className="text-caption">
                  All
                </Link>
              </div>
              {owner.requests.length === 0 ? (
                <p className="text-sm2 text-muted-foreground">No request from this owner.</p>
              ) : (
                <ul className="flex flex-col gap-2 text-sm2">
                  {owner.requests.map((req) => (
                    <li key={req.id} className="flex items-center gap-2">
                      <Badge variant="outline" className={REQUEST_STATE_TONE[req.state]}>{req.state}</Badge>
                      <span className="min-w-0 flex-1 truncate text-muted-foreground">
                        {Object.keys(req.requested).map((d) => dimLabel(d as QuotaDim)).join(", ")}
                      </span>
                      <span className="shrink-0 text-caption text-muted-foreground">
                        {when(new Date(req.createdAt ?? 0).getTime())}
                      </span>
                    </li>
                  ))}
                </ul>
              )}
            </div>

            <div className="border border-border bg-card p-4">
              <div className="mb-3 flex items-center justify-between">
                <span className="text-sm2 font-medium">Detached volumes · {detached.length}</span>
              </div>
              {detached.length === 0 ? (
                <p className="text-sm2 text-muted-foreground">None — every volume here still has a live working copy.</p>
              ) : (
                <ul className="flex flex-col gap-1 text-sm2">
                  {detached.map((v) => (
                    <li key={v.name} className="flex items-center justify-between text-caption text-muted-foreground">
                      <span className="font-mono">{v.display_name}</span>
                      <span>{v.snapshots} snapshots</span>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </div>
        </div>

        <div className="border border-border bg-card p-4">
          <div className="mb-3 flex items-center justify-between">
            <span className="text-sm2 font-medium">Audit trail</span>
            <Link href={`/superadmin/audit?target=${encodeURIComponent(owner.owner)}`} className="text-caption">
              All
            </Link>
          </div>
          {owner.audit.length === 0 ? (
            <p className="text-sm2 text-muted-foreground">No recorded write against this owner.</p>
          ) : (
            <table className="w-full text-left text-sm2">
              <thead>
                <tr className="border-b border-border text-caption text-muted-foreground">
                  <th className="py-2 pr-3 font-medium">When</th>
                  <th className="py-2 pr-3 font-medium">Actor</th>
                  <th className="py-2 pr-3 font-medium">Action</th>
                  <th className="py-2 pr-3 font-medium">Result</th>
                  <th className="py-2 font-medium">Reason</th>
                </tr>
              </thead>
              <tbody>
                {owner.audit.map((a, i) => (
                  <tr key={`${a.ts}-${i}`} className="border-b border-border last:border-0">
                    <td className="py-2 pr-3 text-caption text-muted-foreground">{when(new Date(a.ts).getTime())}</td>
                    <td className="py-2 pr-3 font-mono text-caption">{a.actor}</td>
                    <td className="py-2 pr-3">{a.action}</td>
                    <td className="py-2 pr-3 text-muted-foreground">{a.result}</td>
                    <td className="py-2 text-muted-foreground">{a.reason ?? ""}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>
    </div>
  );
}
