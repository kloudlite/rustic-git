import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { mergeRows } from "@/lib/settings";
import { SettingsTable } from "./settings-table";
import { saveCentralSettings } from "./actions";

export const metadata: Metadata = { title: "Settings" };

/** The Central tab (spec §5/§7): every server/worker/gateway/api tunable, one document, one
 *  writer path — `PUT /superadmin/settings/central` (Task 6), which itself forwards to the server
 *  tier so the object-store document stays the one place that gets written. */
export default async function CentralSettingsPage() {
  const { token } = await requireSuperadmin("/superadmin/settings");
  const [schemaRes, valuesRes, workloadsRes] = await Promise.all([
    api.getSettingsSchema(token),
    api.getCentralSettings(token),
    // `.ok` folded to `[]` rather than surfaced as a page error: workload status is what drives
    // the roll-progress rows, not the settings themselves — losing it should not block editing.
    api.listWorkloads(token),
  ]);

  if (!schemaRes.ok) throw new Error(schemaRes.message);
  if (!valuesRes.ok) throw new Error(valuesRes.message);
  const workloads = workloadsRes.ok ? workloadsRes.value : [];

  const rows = mergeRows(schemaRes.value.central, valuesRes.value, valuesRes.value.updatedBy, valuesRes.value.updatedAt);

  return <SettingsTable rows={rows} workloads={workloads} onSave={saveCentralSettings} />;
}
