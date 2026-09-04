import type { Metadata } from "next";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { settled } from "@/lib/settings";
import { deltaLabel } from "@/lib/history";
import { rollWorkloadAction } from "../actions";
import { PageHeader } from "../page-header";
import { RollTable } from "../roll-table";
import { SignalsTable } from "../signals-table";
import { KpiStrip, KpiTile } from "../ui/kpi";
import { Section } from "../ui/section";
import { EmptyState } from "../ui/data-table";
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
  const [workloadsR, signalsR, restartsS] = await Promise.all([
    api.listWorkloads(token),
    api.adminMonitoringSignals(token),
    api.adminSeries("restarts", { range: "7d", step: "1d" }, token),
  ]);
  if (!workloadsR.ok) throw new Error(workloadsR.message);
  const central = workloadsR.value.filter((w) => w.scope === "central");
  const sig = signalsR.ok ? signalsR.value : null;
  const firing = sig?.signals.filter((s) => s.state === "firing") ?? [];
  const unknown = sig?.signals.filter((s) => s.state === "unknown") ?? [];
  const notReady = central.filter((w) => !settled(w));
  // The scrape reports restarts per workload; the table wants them keyed by name.
  const restarts = Object.fromEntries((sig?.restarts ?? []).map((r) => [r.workload, r.restarts]));

  return (
    <div className="space-y-4">
      <AutoRefresh intervalMs={10_000} />
      <PageHeader
        title="Monitoring"
        purpose="The alert rules from deploy/alerts.md, and every workload a superadmin can roll."
      />
      <KpiStrip>
        <KpiTile label="Firing" value={firing.length} sub={firing[0] ? `oldest ${firing[0].alert}` : "nothing firing"} />
        <KpiTile
          label="Unknown"
          value={unknown.length}
          sub={unknown[0] ? `${unknown[0].alert} has no samples` : "every rule has samples"}
        />
        <KpiTile
          label="Workloads not ready"
          value={notReady.length}
          sub={notReady[0] ? `${notReady[0].name} ${notReady[0].ready} of ${notReady[0].desired}` : "every workload settled"}
        />
        <KpiTile label="Restarts" value={restartsS.summary.last} sub={deltaLabel(restartsS, "restarts")} series={restartsS} />
        <KpiTile
          label="Scrape failures"
          value={sig?.scrape_failures.length ?? 0}
          sub={sig ? `${sig.pods_listed} pods listed` : "signals unavailable"}
        />
      </KpiStrip>
      {/* The scrape is the flakiest read on the page (many pods, a rate window, a timeout). It
          must not take the roll table down with it: the workloads half is what an operator acts
          on, and a failed scrape is a notice, not a blank page. */}
      {signalsR.ok ? (
        <SignalsTable data={signalsR.value} />
      ) : (
        <p className="text-caption text-destructive">Signals unavailable: {signalsR.message}</p>
      )}
      <RollTable workloads={central} onRoll={rollWorkloadAction} restarts={restarts} />
      <Section eyebrow="Signals" title="Active silences" count={0}>
        <EmptyState>
          No rule is silenced. A silence hides a firing signal from this page and from the overview until it expires.
        </EmptyState>
      </Section>
    </div>
  );
}
