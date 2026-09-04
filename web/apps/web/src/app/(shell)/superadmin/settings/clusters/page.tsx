import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { mergeRows } from "@/lib/settings";
import { NavTabs } from "@/components/app/nav-tabs";
import { SettingsTable } from "../settings-table";
import { saveClusterSettings, revertClusterSettingsAction } from "../actions";

export const metadata: Metadata = { title: "Cluster settings" };

/** The Clusters tab: one panel per region, spec §5/§7. Region selection is a plain query param —
 *  reusing `NavTabs` (a `Link` row) rather than a client picker keeps region switches an ordinary
 *  navigation, so the panel that loads is always the server-rendered one for that region, never a
 *  client fetch that could show one region's rows under another's URL. */
export default async function ClustersSettingsPage({
  searchParams,
}: {
  searchParams: Promise<{ region?: string }>;
}) {
  const { token } = await requireSuperadmin("/superadmin/settings/clusters");
  const { region: qRegion } = await searchParams;

  const [regionsRes, schemaRes] = await Promise.all([api.listRegions(token), api.getSettingsSchema(token)]);
  const regions = regionsRes.ok ? regionsRes.value : [];
  if (!schemaRes.ok) throw new Error(schemaRes.message);

  if (regions.length === 0) {
    return <p className="text-sm2 text-muted-foreground">No regions yet.</p>;
  }
  const region = qRegion && regions.some((r) => r.id === qRegion) ? qRegion : regions[0].id;

  const [valuesRes, workloadsRes] = await Promise.all([
    api.getClusterSettings(region, token),
    api.listWorkloads(token),
  ]);
  if (!valuesRes.ok) throw new Error(valuesRes.message);
  const workloads = workloadsRes.ok ? workloadsRes.value : [];

  const ann = valuesRes.value.metadata?.annotations ?? {};
  const rows = mergeRows(
    schemaRes.value.cluster,
    valuesRes.value.spec,
    ann["rustic-git.io/updated-by"],
    ann["rustic-git.io/updated-at"],
  );

  return (
    <div className="space-y-6">
      <NavTabs
        aria-label="Region"
        activeHref={`/superadmin/settings/clusters?region=${region}`}
        tabs={regions.map((r) => ({ href: `/superadmin/settings/clusters?region=${r.id}`, label: r.id, exact: true }))}
      />
      <SettingsTable
        key={region}
        rows={rows}
        workloads={workloads}
        onSave={(patch) => saveClusterSettings(region, patch)}
        onRevert={() => revertClusterSettingsAction(region)}
      />
    </div>
  );
}
