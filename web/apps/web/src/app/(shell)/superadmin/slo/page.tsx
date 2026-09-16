import type { Metadata } from "next";
import Link from "next/link";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { when } from "@/lib/time";
import { budgetLabel, jobDone, jobsOf, msLabel, runGlyph, runStateLabel, runTone, type SloJob } from "@/lib/slo";
import { PageHeader } from "../page-header";
import { Section } from "../ui/section";
import { KpiStrip, KpiTile } from "../ui/kpi";
import { EmptyState } from "../ui/data-table";
import { SloTable } from "./slo-table";
import { RunsTable } from "./runs-table";
import { RunTree, Glyph } from "./run-tree";
import { Pill } from "../ui/pill";

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
export default async function SloPage({ searchParams }: { searchParams: Promise<{ idle?: string }> }) {
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
  const { slos, runs } = o.value;
  // The idle panel is otherwise unreachable for review: the fixtures always have a run in flight.
  // Fixtures only — against a real admin api this flag does nothing.
  const idle = process.env.KLOUDLITE_ADMIN_FIXTURES === "1" && (await searchParams).idle === "1";
  const running = idle ? [] : o.value.running;
  // The hourly suite is four parallel group runs; an operator asks about the JOB, so the panel and
  // both KPIs count jobs. `running` alone would draw a job of two once half its groups have
  // finished, so the folding sees the recent runs as well — by run id, `running` winning, because
  // it is the fresher of the two reads.
  const byId = new Map([...(idle ? runs.filter((r) => r.state !== "running") : runs), ...running].map((r) => [r.run_id, r]));
  const jobs = jobsOf([...byId.values()]);
  const live = jobs.filter((j) => j.state === "running");
  const finished = jobs.filter((j) => j.state !== "running");
  // Idle is never an empty box: the panel falls back to the last finished job, collapsed.
  const shown = live.length > 0 ? live : finished.slice(0, 1);
  // A job of one is the single-run panel, tree and all; a grouped job draws its groups from the
  // run rows alone, so the extra reads stay at one per ungrouped job in flight.
  const details = new Map(
    (
      await Promise.all(
        shown
          .filter((j) => j.runs.length === 1)
          .map(async (j) => [j.key, await api.adminSloRun(token, j.runs[0].run_id)] as const),
      )
    ).map(([k, d]) => [k, d.ok ? d.value : null]),
  );

  const burning = slos.filter((s) => s.state === "burning" || s.state === "breaching");
  // "Today" is the calendar day the operator is looking at, in their own zone — the same day a
  // pager would have woken them.
  const midnight = new Date();
  midnight.setHours(0, 0, 0, 0);
  const failedToday = finished.filter((j) => j.state === "failed" && new Date(j.started) >= midnight);
  // The one closest to running out, ignoring the SLOs with no samples at all.
  const lowest = slos.filter((s) => s.budget_left != null).sort((a, b) => a.budget_left! - b.budget_left!)[0];
  const last = finished[0];

  return (
    <div className="space-y-4">
      <AutoRefresh intervalMs={10_000} />
      <PageHeader title="SLOs" purpose="What the probe measures, and how much error budget is left." />

      <KpiStrip cols={4}>
        <KpiTile
          label="Running now"
          value={running.length > 0 ? `${running.length} runs` : "idle"}
          sub={
            running.length > 0
              ? `across ${live.length} ${live.length === 1 ? "job" : "jobs"} · ${live.map((j) => j.suite).join(", ")}`
              : last
                ? `last job ${runStateLabel(last.state)} ${when(new Date(last.started).getTime())}`
                : "no run has reported yet"
          }
        />
        <KpiTile
          label="Jobs failed today"
          value={failedToday.length}
          sub={
            failedToday[0]
              ? failedToday[0].runs.find((r) => r.failed_step)?.failed_step || failedToday[0].runs[0].stage
              : "every job today passed"
          }
        />
        <KpiTile
          label="SLOs burning"
          value={burning.length}
          sub={burning[0] ? `${burning[0].id} is ${burning[0].state}` : "every SLO is inside its budget"}
        />
        <KpiTile
          label="Lowest budget"
          value={lowest ? budgetLabel(lowest.budget_left, lowest.budget_30d) : "—"}
          sub={lowest ? lowest.id : "no SLO has a sample in the window"}
        />
      </KpiStrip>

      {/* The journey the probe walks, live while it walks it — and when nothing is in flight, the
          last job's own tree rather than an empty box that reads as a broken probe. */}
      {shown.map((job) => (
        <JobPanel
          key={job.key}
          job={job}
          live={live.length > 0}
          detail={details.get(job.key) ?? null}
          slos={slos}
        />
      ))}

      <SloTable slos={slos} />
      <RunsTable runs={runs} />
    </div>
  );
}

/** One launch of the probe. A job of one group renders the run tree it always did; a job of four
 *  renders one line per group, because four trees on one screen is four screens. */
function JobPanel({
  job,
  live,
  detail,
  slos,
}: {
  job: SloJob;
  live: boolean;
  detail: api.SloRunDetail | null;
  slos: api.SloStatus[];
}) {
  const { done, total } = jobDone(job);
  const solo = job.runs.length === 1 ? job.runs[0] : null;
  return (
    <Section
      eyebrow="Probe"
      title={live ? `Running · ${job.suite}` : `Last job · ${job.suite}`}
      count={total > 1 ? `${done} of ${total} groups done` : live ? undefined : `${runStateLabel(job.state)} ${when(new Date(job.started).getTime())}`}
      bare
      toolbar={
        solo && (
          <Link
            href={`/superadmin/slo/runs/${encodeURIComponent(solo.run_id)}`}
            className="text-caption text-primary underline-offset-4 hover:underline"
          >
            Open run
          </Link>
        )
      }
    >
      {solo ? (
        detail ? (
          <RunTree run={solo} steps={detail.steps} journey={detail.journey} slos={slos} />
        ) : (
          <EmptyState>{solo.stage} has not reported a step yet.</EmptyState>
        )
      ) : (
        <ul>
          {job.runs.map((r) => (
            <li
              key={r.run_id}
              className="grid grid-cols-[2.25rem_minmax(0,1fr)_6rem_7rem_8rem] items-baseline gap-3 border-b border-border px-4 py-2 last:border-b-0"
            >
              <span className="text-right text-caption tabular-nums text-muted-foreground">g{r.group ?? 0}</span>
              <span className="flex min-w-0 items-baseline gap-2">
                <Glyph state={runGlyph(r.state)} className="translate-y-0.5" />
                <Link
                  href={`/superadmin/slo/runs/${encodeURIComponent(r.run_id)}`}
                  className="truncate text-sm2 font-medium text-primary underline-offset-4 hover:underline"
                >
                  {r.stage}
                </Link>
              </span>
              <span className="text-right text-caption tabular-nums text-muted-foreground">
                {r.steps_failed > 0 ? `${r.steps_failed} failed` : `${r.steps_total} steps`}
              </span>
              <span className="text-right text-sm2 tabular-nums text-muted-foreground">{msLabel(r.duration_ms)}</span>
              <span className="flex justify-end">
                <Pill tone={runTone(r.state)}>{runStateLabel(r.state)}</Pill>
              </span>
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}
