import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { rollWorkloadAction } from "../actions";
import { PageHeader } from "../page-header";
import { RollTable } from "../roll-table";
import { SignalsTable } from "../signals-table";
import { AutoRefresh } from "@/components/app/auto-refresh";

export const metadata: Metadata = { title: "Monitoring" };

/** The CENTRAL workloads only — server, worker, gateway, api — each region's agent DaemonSet and
 *  gateway live on the Clusters tab instead, next to the nodes they run on.
 *
 *  The signals scrape can take several seconds (`SCRAPE_TIMEOUT` plus a rate window). `AutoRefresh`
 *  re-runs this server component in a transition, which keeps the existing table on screen until
 *  the new one is ready — never a blank page while polling. */
export default async function MonitoringPage() {
  const { token } = await requireSuperadmin("/superadmin/monitoring");
  const [workloadsR, signalsR] = await Promise.all([api.listWorkloads(token), api.adminMonitoringSignals(token)]);
  if (!workloadsR.ok) throw new Error(workloadsR.message);
  const central = workloadsR.value.filter((w) => w.scope === "central");

  return (
    <div className="space-y-6">
      <AutoRefresh intervalMs={10_000} />
      <PageHeader title="Monitoring" purpose="Central workload image, rollout state, the manual roll, and the alert catalogue." />
      <RollTable workloads={central} onRoll={rollWorkloadAction} />
      {/* The scrape is the flakiest read on the page (many pods, a rate window, a timeout). It
          must not take the roll table down with it: the workloads half is what an operator acts
          on, and a failed scrape is a notice, not a blank page. */}
      {signalsR.ok ? (
        <SignalsTable data={signalsR.value} />
      ) : (
        <p className="text-caption text-destructive">Signals unavailable: {signalsR.message}</p>
      )}
    </div>
  );
}
