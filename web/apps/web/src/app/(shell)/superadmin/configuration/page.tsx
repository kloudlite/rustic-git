import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import type { SettingsSchemaRow } from "@/lib/api";
import { effectiveValue, fmt } from "@/lib/settings";
import { PageHeader } from "../page-header";

export const metadata: Metadata = { title: "Configuration" };

/** One scope's rows, each carrying its own effective value against `stored` — a dynamic document
 *  keyed by the same camelCase names the schema rows already use. No form controls anywhere on
 *  this page — read-only per spec; the sentence at the top says where a value actually changes. */
function SchemaTable({ rows, stored }: { rows: SettingsSchemaRow[]; stored: Record<string, unknown> }) {
  if (rows.length === 0) {
    return (
      <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
        Nothing here yet.
      </p>
    );
  }
  return (
    <div className="overflow-x-auto border border-border bg-card">
      <table className="w-full text-sm2">
        <thead>
          <tr className="border-b border-border text-left text-caption text-muted-foreground">
            <th className="px-3 py-2 font-normal">Name</th>
            <th className="px-3 py-2 font-normal">Description</th>
            <th className="px-3 py-2 font-normal">Unit</th>
            <th className="px-3 py-2 font-normal">Range</th>
            <th className="px-3 py-2 font-normal">Value</th>
            <th className="px-3 py-2 font-normal">Source</th>
            <th className="px-3 py-2 font-normal">Default</th>
            <th className="px-3 py-2 font-normal">Env override</th>
            <th className="px-3 py-2 font-normal">Mark</th>
            <th className="px-3 py-2 font-normal">Readers</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const { value, source } = effectiveValue(stored[row.name], row.env, row.default);
            return (
              <tr key={row.name} className="border-b border-border last:border-0">
                <td className="px-3 py-2 font-medium">{row.name}</td>
                <td className="px-3 py-2 text-muted-foreground">{row.description}</td>
                <td className="px-3 py-2 tabular-nums">{row.unit}</td>
                <td className="px-3 py-2 tabular-nums">{row.range ? `${row.range.min}–${row.range.max}` : "—"}</td>
                <td className="px-3 py-2 tabular-nums font-medium">{fmt(value)}</td>
                <td className="px-3 py-2">{source}</td>
                <td className="px-3 py-2 tabular-nums text-muted-foreground">{fmt(row.default)}</td>
                <td className="px-3 py-2 tabular-nums text-muted-foreground">{fmt(row.env)}</td>
                <td className="px-3 py-2">{row.mark}</td>
                <td className="px-3 py-2 text-muted-foreground">{row.readers.length ? row.readers.join(", ") : "—"}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

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

  return (
    <div className="space-y-8">
      <PageHeader title="Configuration" purpose="Effective tunables, per scope, and where each is defined." />

      {firstError && !firstError.ok && (
        <p className="border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 text-destructive">
          {firstError.message}
        </p>
      )}

      <p className="text-sm2 text-muted-foreground">
        Stored values are changed through the admin API directly (<code>PUT /admin/settings/central</code> /{" "}
        <code>/admin/settings/clusters/{"{region}"}</code>); this page is read-only. A boot-marked field takes
        effect on the next roll of its readers; a live one takes effect on their next refresh beat.
      </p>

      <div>
        <h2 className="mb-2 text-sm2 font-medium">Central</h2>
        <SchemaTable rows={schema.central} stored={central} />
      </div>

      {regions.map((rg, i) => {
        const res = clusterSettled[i];
        const spec = res?.ok ? res.value.spec : {};
        return (
          <div key={rg.id}>
            <h2 className="mb-2 text-sm2 font-medium">Cluster — {rg.id}</h2>
            {res && !res.ok && (
              <p className="mb-2 border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 text-destructive">
                {res.message}
              </p>
            )}
            <SchemaTable rows={schema.cluster} stored={spec} />
          </div>
        );
      })}
    </div>
  );
}
