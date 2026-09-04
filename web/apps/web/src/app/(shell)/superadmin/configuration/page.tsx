import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { PageHeader } from "../page-header";
import { KpiTile } from "../ui/kpi";
import { ScopeTables, type Scope } from "./scope-table";

export const metadata: Metadata = { title: "Configuration" };

export default async function ConfigurationPage() {
  const { token } = await requireSuperadmin("/superadmin/configuration");
  const [schemaRes, centralRes, regionsRes] = await Promise.all([
    api.adminSettingsSchema(token),
    api.adminCentralSettings(token),
    api.listRegions(token),
  ]);
  const schema = schemaRes.ok ? schemaRes.value : { central: [], cluster: [] };
  const central = centralRes.ok ? centralRes.value : {};
  const regions = regionsRes.ok ? regionsRes.value : [];

  // One `GET /admin/settings/clusters/{region}` per region — every region shares the same
  // `cluster` schema rows, only the stored document (and so the effective value) differs.
  const clusterSettled = await Promise.all(regions.map((rg) => api.adminClusterSettings(rg.id, token)));

  const firstError = [schemaRes, centralRes, regionsRes].find((r) => !r.ok);
  const scopes: Scope[] = [
    {
      title: "Central · cluster/settings",
      readers: "Read by server, api, worker, gateway · refreshed on a 30 s beat",
      rows: schema.central,
      stored: central,
    },
    ...regions.map((rg, i) => {
      const res = clusterSettled[i];
      return {
        title: `Cluster · ${rg.id} · ClusterSettings/default`,
        readers: "Read by kloudlite-git-agent on every node · watched through a reflector",
        rows: schema.cluster,
        stored: res?.ok ? res.value.spec : {},
        error: res && !res.ok ? res.message : null,
      };
    }),
  ];

  return (
    <div className="space-y-4">
      <AutoRefresh intervalMs={10_000} />
      <PageHeader
        title="Configuration"
        purpose="Every knob, where its current value comes from, and what changing it would cost. Read-only here."
      />
      {firstError && !firstError.ok && (
        <p className="border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 text-destructive">
          {firstError.message}
        </p>
      )}
      <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
        {/* The page's whole reason to exist: three places a value can come from, said once at the
            top, so nobody has to guess which one produced the number in the table below. */}
        <KpiTile label="Deploy manifest" value="deploy/" sub="Image pins, replicas, ingress hosts. Never editable from this console." />
        <KpiTile label="Environment" value="env" sub="The bootstrap value a fresh cluster starts with. Read only when nothing is stored." />
        <KpiTile label="Stored" value="admin API" sub="Versioned and revertable. Last ten versions kept per scope." />
      </div>
      <ScopeTables scopes={scopes} />
    </div>
  );
}
