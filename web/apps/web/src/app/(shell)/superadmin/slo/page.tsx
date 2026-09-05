import type { Metadata } from "next";
import Link from "next/link";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { when } from "@/lib/time";
import { budgetLabel } from "@/lib/slo";
import { PageHeader } from "../page-header";
import { Section } from "../ui/section";
import { KpiStrip, KpiTile } from "../ui/kpi";
import { EmptyState } from "../ui/data-table";
import { SloTable } from "./slo-table";
import { RunsTable } from "./runs-table";
import { RunTracker, seconds } from "./run-tracker";

export const metadata: Metadata = { title: "SLOs" };

/** The probe's own screen: what the catalogue promises, what the last thirty days actually did,
 *  and the run in flight.
 *
 *  Everything comes from one `/admin/slo`, which is why the 10 s poll is affordable. The running
 *  run's STEPS are a second read — the overview carries the run row, not its steps — and only
 *  while something is running, so an idle console still makes one request per poll.
 *
 *  With no ClickHouse the whole area is `503 history unavailable`, exactly like `/admin/history/*`:
 *  a placeholder saying so, never a page of zeroes that would read as a probe reporting success. */
export default async function SloPage() {
  const { token } = await requireSuperadmin("/superadmin/slo");
  const o = await api.adminSlo(token);
  if (!o.ok) {
    return (
      <div className="space-y-4">
        <PageHeader title="SLOs" purpose="What the probe measures, and how much error budget is left." />
        <Section eyebrow="Probe" title="Service level objectives">
          <EmptyState>The history layer is unavailable, so nothing has been measured: {o.message}</EmptyState>
        </Section>
      </div>
    );
  }
  const { slos, running, runs } = o.value;
  const detail = running ? await api.adminSloRun(token, running.run_id) : null;

  const burning = slos.filter((s) => s.state === "burning" || s.state === "breaching");
  // "Today" is the calendar day the operator is looking at, in their own zone — the same day a
  // pager would have woken them.
  const midnight = new Date();
  midnight.setHours(0, 0, 0, 0);
  const failedToday = runs.filter((r) => r.state === "failed" && new Date(r.started) >= midnight);
  // The one closest to running out, ignoring the SLOs with no samples at all.
  const lowest = slos.filter((s) => s.budget_left != null).sort((a, b) => a.budget_left! - b.budget_left!)[0];
  const last = runs.find((r) => r.state !== "running");

  return (
    <div className="space-y-4">
      <AutoRefresh intervalMs={10_000} />
      <PageHeader title="SLOs" purpose="What the probe measures, and how much error budget is left." />

      <KpiStrip cols={4}>
        <KpiTile
          label="Running now"
          value={running ? running.stage : "idle"}
          sub={running ? `${running.suite} · ${seconds(running.duration_ms)} so far` : last ? `last run ${last.state} ${when(new Date(last.started).getTime())}` : "no run has reported yet"}
        />
        <KpiTile
          label="Runs failed today"
          value={failedToday.length}
          sub={failedToday[0] ? `${failedToday[0].failed_step || failedToday[0].stage}` : "every run today passed"}
        />
        <KpiTile
          label="SLOs burning"
          value={burning.length}
          sub={burning[0] ? `${burning[0].id} is ${burning[0].state}` : "every SLO is inside its budget"}
        />
        <KpiTile
          label="Lowest budget"
          value={lowest ? budgetLabel(lowest.budget_left) : "—"}
          sub={lowest ? lowest.id : "no SLO has a sample in the window"}
        />
      </KpiStrip>

      {/* Hidden when idle: an empty tracker is a row of nothing that reads as a broken probe. */}
      {running && (
        <Section
          eyebrow="Probe"
          title={`Running · ${running.suite}`}
          count={`${running.steps_total} steps`}
          toolbar={
            <Link
              href={`/superadmin/slo/runs/${encodeURIComponent(running.run_id)}`}
              className="text-caption text-primary underline-offset-4 hover:underline"
            >
              Open run
            </Link>
          }
        >
          {detail?.ok && detail.value.steps.length > 0 ? (
            <RunTracker run={running} steps={detail.value.steps} />
          ) : (
            <EmptyState>{running.stage} has not reported a step yet.</EmptyState>
          )}
        </Section>
      )}

      <SloTable slos={slos} />
      <RunsTable runs={runs} />
    </div>
  );
}
