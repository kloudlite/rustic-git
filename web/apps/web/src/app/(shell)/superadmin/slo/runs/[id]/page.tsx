import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { requireSuperadmin } from "@/lib/session";
import * as api from "@/lib/api";
import { when } from "@/lib/time";
import { msLabel, treeOf } from "@/lib/slo";
import { PageHeader } from "../../../page-header";
import { Section } from "../../../ui/section";
import { KpiStrip, KpiTile } from "../../../ui/kpi";
import { EmptyState } from "../../../ui/data-table";
import { RunTree } from "../../run-tree";

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
  // The catalogue too: the tree names every step by its SLI sentence, and a run alone carries ids.
  const [r, overview] = await Promise.all([api.adminSloRun(token, id), api.adminSlo(token)]);
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
  const slos = overview.ok ? overview.value.slos : [];
  const tree = treeOf(run.journey, run.steps);
  const hyperdx = process.env.KLOUDLITE_GIT_HYPERDX_URL;
  const search = `service.name:kloudlite-git-slo run_id:${run.run_id}`;

  return (
    <div className="space-y-4">
      <PageHeader title={`Run ${run.run_id}`} purpose={`${run.suite} suite in ${run.region}, ${when(new Date(run.started).getTime())}.`} />

      <KpiStrip cols={4}>
        <KpiTile label="State" value={run.state} sub={run.failed_step ? `failed at ${run.failed_step}` : `stage ${run.stage}`} />
        <KpiTile label="Duration" value={msLabel(run.duration_ms)} sub={run.finished ? "finished" : "still running"} />
        <KpiTile label="Steps" value={run.steps_total} sub={`${run.steps_failed} failed, ${run.steps.filter((s) => s.skipped).length} skipped`} />
        <KpiTile label="Stages" value={tree.length} sub={`${tree.filter((s) => s.state === "passed").length} passed, ${tree.filter((s) => s.state === "skipped").length} skipped`} />
      </KpiStrip>

      {run.failed_detail && (
        <p className="border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 text-destructive">
          {run.failed_step}: {run.failed_detail}
        </p>
      )}

      <Section
        bare
        eyebrow="Journey"
        title="Stages"
        count={tree.length}
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
        {tree.length === 0 ? (
          <EmptyState>This run reported no step.</EmptyState>
        ) : (
          <RunTree run={run} steps={run.steps} journey={run.journey} slos={slos} expanded />
        )}
      </Section>

    </div>
  );
}
