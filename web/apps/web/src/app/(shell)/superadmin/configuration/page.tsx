import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import type { SettingsSchemaRow } from "@/lib/api";
import { PageHeader } from "../page-header";

export const metadata: Metadata = { title: "Configuration" };

function fmt(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (typeof v === "boolean") return v ? "true" : "false";
  return String(v);
}

/** One scope's rows. No form controls anywhere on this page — read-only per spec; the sentence
 *  at the top says where a value actually changes. */
function SchemaTable({ rows }: { rows: SettingsSchemaRow[] }) {
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
            <th className="px-3 py-2 font-normal">Default</th>
            <th className="px-3 py-2 font-normal">Env override</th>
            <th className="px-3 py-2 font-normal">Mark</th>
            <th className="px-3 py-2 font-normal">Readers</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.name} className="border-b border-border last:border-0">
              <td className="px-3 py-2 font-medium">{row.name}</td>
              <td className="px-3 py-2 text-muted-foreground">{row.description}</td>
              <td className="px-3 py-2 tabular-nums">{row.unit}</td>
              <td className="px-3 py-2 tabular-nums">{row.range ? `${row.range.min}–${row.range.max}` : "—"}</td>
              <td className="px-3 py-2 tabular-nums">{fmt(row.default)}</td>
              <td className="px-3 py-2 tabular-nums">{fmt(row.env)}</td>
              <td className="px-3 py-2">{row.mark}</td>
              <td className="px-3 py-2 text-muted-foreground">{row.readers.length ? row.readers.join(", ") : "—"}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export default async function ConfigurationPage() {
  const { token } = await requireSuperadmin("/superadmin/configuration");
  const r = await api.adminSettingsSchema(token);
  const schema = r.ok ? r.value : { central: [], cluster: [] };

  return (
    <div className="space-y-8">
      <PageHeader title="Configuration" purpose="Effective tunables, per scope, and where each is defined." />

      {!r.ok && (
        <p className="border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 text-destructive">
          {r.message}
        </p>
      )}

      <p className="text-sm2 text-muted-foreground">
        Stored values are changed through the admin API directly (<code>PUT /admin/settings/central</code> /{" "}
        <code>/admin/settings/clusters/{"{region}"}</code>); this page is read-only. A boot-marked field takes
        effect on the next roll of its readers; a live one takes effect on their next refresh beat.
      </p>

      <div>
        <h2 className="mb-2 text-sm2 font-medium">Central</h2>
        <SchemaTable rows={schema.central} />
      </div>

      <div>
        <h2 className="mb-2 text-sm2 font-medium">Cluster</h2>
        <SchemaTable rows={schema.cluster} />
      </div>
    </div>
  );
}
