import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { rollWorkloadAction } from "../actions";
import { PageHeader } from "../page-header";
import { RollTable } from "../roll-table";

export const metadata: Metadata = { title: "Monitoring" };

/** The CENTRAL workloads only — server, worker, gateway, api — each region's agent DaemonSet and
 *  gateway live on the Clusters tab instead, next to the nodes they run on. */
export default async function MonitoringPage() {
  const { token } = await requireSuperadmin("/superadmin/monitoring");
  const r = await api.listWorkloads(token);
  if (!r.ok) throw new Error(r.message);
  const central = r.value.filter((w) => w.scope === "central");

  return (
    <div>
      <PageHeader title="Monitoring" purpose="Central workload image, rollout state, and the manual roll." />
      <RollTable workloads={central} onRoll={rollWorkloadAction} />
    </div>
  );
}
