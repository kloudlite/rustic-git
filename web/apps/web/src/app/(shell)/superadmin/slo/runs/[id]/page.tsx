import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { when } from "@/lib/time";
import { stagesOf } from "@/lib/slo";
import { PageHeader } from "../../../page-header";
import { Section } from "../../../ui/section";
import { KpiStrip, KpiTile } from "../../../ui/kpi";
import { DataTable, Td, Th, Tr, EmptyState } from "../../../ui/data-table";
import { Pill } from "../../../ui/pill";
import { RunTracker, seconds } from "../../run-tracker";

export const metadata: Metadata = { title: "Probe run" };

/** One run, every step in the order it ran, under the journey stage it ran in.
 *
 *  ponytail: the "Open in HyperDX" link needs `KLOUDLITE_GIT_HYPERDX_URL` on the WEB deployment —
 *  `deploy/kloudlite-git-web.yaml` does not set it today, so the link is simply absent there, the
 *  same way `/admin/monitoring` omits it when the admin process has none. Unset means no link,
 *  never a dead one. */
export default async function RunPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  const { token } = await requireSuperadmin(`/superadmin/slo/runs/${id}`);
  const r = await api.adminSloRun(token, id);
  if (!r.ok) {
    if (r.kind === "notFound") notFound();
    return (
      <div className="space-y-4">
        <PageHeader title="Probe run" purpose={id} />
        <Section eyebrow="Probe" title="Run">
          <EmptyState>This run could not be read: {r.message}</EmptyState>
        </Section>
      </div>
    );
  }
  const run = r.value;
  const stages = stagesOf(run.steps);
  const hyperdx = process.env.KLOUDLITE_GIT_HYPERDX_URL;
  const search = `service.name:kloudlite-git-slo run_id:${run.run_id}`;

  return (
    <div className="space-y-4">
      <PageHeader title={`Run ${run.run_id}`} purpose={`${run.suite} suite in ${run.region}, ${when(new Date(run.started).getTime())}.`} />

      <KpiStrip cols={4}>
        <KpiTile label="State" value={run.state} sub={run.failed_step ? `failed at ${run.failed_step}` : `stage ${run.stage}`} />
        <KpiTile label="Duration" value={seconds(run.duration_ms)} sub={run.finished ? "finished" : "still running"} />
        <KpiTile label="Steps" value={run.steps_total} sub={`${run.steps_failed} failed, ${run.steps.filter((s) => s.skipped).length} skipped`} />
        <KpiTile label="Stages" value={stages.length} sub={stages.map((s) => s.stage).join(" · ") || "no step reported"} />
      </KpiStrip>

      {run.failed_detail && (
        <p className="border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 text-destructive">
          {run.failed_step}: {run.failed_detail}
        </p>
      )}

      <Section
        eyebrow="Journey"
        title="Stages"
        count={stages.length}
        toolbar={
          <div className="flex items-center gap-2">
            {hyperdx && (
              <a
                href={`${hyperdx.replace(/\/$/, "")}/search?q=${encodeURIComponent(search)}`}
                target="_blank"
                rel="noreferrer"
                className="inline-flex h-8 items-center border border-border px-3 text-sm2 hover:bg-muted"
              >
                Open in HyperDX
              </a>
            )}
            <Link href="/superadmin/slo" className="text-caption text-primary underline-offset-4 hover:underline">
              All SLOs
            </Link>
          </div>
        }
      >
        {stages.length === 0 ? <EmptyState>This run reported no step.</EmptyState> : <RunTracker run={run} steps={run.steps} />}
      </Section>

      <Section eyebrow="Probe" title="Steps" count={run.steps.length} bare>
        {run.steps.length === 0 ? (
          <EmptyState>This run reported no step.</EmptyState>
        ) : (
          <DataTable>
            <thead>
              <tr>
                <Th>Stage</Th>
                <Th>SLO</Th>
                <Th>At</Th>
                <Th numeric>ms</Th>
                <Th>Result</Th>
                <Th>Detail</Th>
              </tr>
            </thead>
            <tbody>
              {run.steps.map((s) => (
                <Tr key={`${s.slo_id}-${s.ts}`}>
                  <Td className="text-muted-foreground">{s.stage}</Td>
                  <Td className="font-mono text-caption">{s.slo_id}</Td>
                  <Td className="text-muted-foreground tabular-nums">{new Date(s.ts).toISOString().slice(11, 19)}</Td>
                  <Td numeric>{s.ms}</Td>
                  <Td>
                    <Pill tone={s.skipped ? "neutral" : s.ok ? "ok" : "critical"}>
                      {s.skipped ? "skipped" : s.ok ? "ok" : "failed"}
                    </Pill>
                  </Td>
                  <Td className="text-muted-foreground">{s.detail || "—"}</Td>
                </Tr>
              ))}
            </tbody>
          </DataTable>
        )}
      </Section>
    </div>
  );
}
