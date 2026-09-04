import type { Metadata } from "next";
import Link from "next/link";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { createRegionAction } from "../actions";
import { PageHeader } from "../page-header";
import { RegionStatusBadge, SettingsStatusBadge } from "../status-badge";
import { RegionStatusToggle } from "./region-status";

export const metadata: Metadata = { title: "Clusters" };

/** One card per region — everything the mockup (`Clusters.dc.html`) asks for without a second
 *  click: agents ready/desired, nodes ready/total plus how many are draining, live working
 *  copies, and whether `ClusterSettings/default` exists for it. Open for the node table. */
export default async function ClustersPage() {
  const { token } = await requireSuperadmin("/superadmin/clusters");
  const r = await api.adminClusters(token);
  const rows = r.ok ? r.value : [];

  return (
    <div className="space-y-8">
      <PageHeader title="Clusters" purpose="Every region and how it is doing. Open one for its nodes and workloads." />
      {!r.ok && <p className="text-sm2 text-destructive">{r.message}</p>}

      <AutoRefresh />

      <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
        {rows.map((c) => (
          <div key={c.region} className="flex flex-col gap-3 border border-border bg-card p-4">
            <div className="flex items-center justify-between gap-3">
              <div className="flex items-center gap-2">
                <Link href={`/superadmin/clusters/${encodeURIComponent(c.region)}`} className="font-mono text-sm2 font-medium">
                  {c.region}
                </Link>
                <RegionStatusBadge status={c.status} />
              </div>
              <div className="flex items-center gap-2">
                <RegionStatusToggle region={c.region} status={c.status} />
                <Link href={`/superadmin/clusters/${encodeURIComponent(c.region)}`} className="text-caption text-primary">
                  Open
                </Link>
              </div>
            </div>
            <div className="grid grid-cols-4 gap-3 text-sm2">
              <div>
                <div className="text-caption text-muted-foreground">Agents</div>
                <div className="font-medium tabular-nums">{c.agentsReady} / {c.agentsDesired}</div>
              </div>
              <div>
                <div className="text-caption text-muted-foreground">Nodes</div>
                <div className="font-medium tabular-nums">
                  {c.nodesReady} / {c.nodesTotal}
                  {c.draining > 0 && <span className="ml-1 text-warning">· {c.draining} draining</span>}
                </div>
              </div>
              <div>
                <div className="text-caption text-muted-foreground">Live copies</div>
                <div className="font-medium tabular-nums">{c.workingCopies}</div>
              </div>
              <div>
                <div className="text-caption text-muted-foreground">Settings</div>
                <SettingsStatusBadge status={c.settingsStatus} />
              </div>
            </div>
          </div>
        ))}
        {rows.length === 0 && (
          <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground md:col-span-2">
            No regions yet — add one below.
          </p>
        )}
      </div>

      <form action={createRegionAction} className="flex items-end gap-3 border border-border bg-card p-4">
        <label className="grid gap-1 text-sm2">
          Id
          <Input name="id" required className="h-8" />
        </label>
        <label className="grid gap-1 text-sm2">
          Name
          <Input name="name" required className="h-8" />
        </label>
        <Button type="submit" size="sm">
          Add region
        </Button>
      </form>
    </div>
  );
}
