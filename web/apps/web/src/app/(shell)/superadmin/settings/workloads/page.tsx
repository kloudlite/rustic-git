import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { WorkloadsTable } from "./workloads-table";
import { rollWorkloadAction } from "../actions";

export const metadata: Metadata = { title: "Workloads" };

/** The Workloads tab (spec §6): every known workload's image/rollout/last-roll, plus each node's
 *  decommission status, entirely read-only except the manual roll button — no other write exists
 *  on this route, matching `crates/workspaces/src/api/admin.rs`'s roll handler being the only POST
 *  under `/admin/workloads`. */
export default async function WorkloadsPage() {
  const { token } = await requireSuperadmin("/superadmin/settings/workloads");
  const [workloadsRes, nodesRes, centralRes] = await Promise.all([
    api.listWorkloads(token),
    api.adminListNodes(token),
    // Same document the Central tab already renders — the hosts block here is a read-only slice
    // of it, not a second source of truth, so no new backend route.
    api.getCentralSettings(token),
  ]);

  if (!workloadsRes.ok) throw new Error(workloadsRes.message);
  const nodes = nodesRes.ok ? nodesRes.value : [];
  const central = centralRes.ok ? centralRes.value : {};

  return (
    <WorkloadsTable
      workloads={workloadsRes.value}
      nodes={nodes}
      hosts={{
        cloneHost: central.cloneHost,
        sshHost: central.sshHost,
        sshPort: central.sshPort,
        registryHost: central.registryHost,
      }}
      onRoll={rollWorkloadAction}
    />
  );
}
