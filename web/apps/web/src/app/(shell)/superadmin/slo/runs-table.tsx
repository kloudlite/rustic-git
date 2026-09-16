"use client";

import { useState } from "react";
import Link from "next/link";
import type { SloRun } from "@/lib/api";
import { when } from "@/lib/time";
import { Section } from "../ui/section";
import { DataTable, Td, Th, Tr, EmptyState } from "../ui/data-table";
import { Pill } from "../ui/pill";
import { jobsOf, msLabel, runGlyph, runStateLabel, runTone, type SloJob } from "@/lib/slo";
import { Glyph } from "./run-tree";

/** The last twenty probe runs, folded into the JOBS that launched them — the hourly suite is four
 *  parallel group runs, and four rows saying "hourly" is a table nobody can read a pass out of.
 *  A job of one (every other suite) is the row it always was.
 *
 *  A client component only for the disclosure: the per-run rows are the ones that existed here
 *  before, kept one click away rather than deleted.
 *
 *  The failure detail is truncated to one line on purpose — it is a human-written sentence and can
 *  be two thousand characters; the run page shows all of it. */
export function RunsTable({ runs }: { runs: SloRun[] }) {
  const [open, setOpen] = useState<string[]>([]);
  // The job in flight first, whatever order the api answered in: it is the only row still
  // changing, and a reader scanning for it should never have to look past the first line.
  const jobs = jobsOf(runs).sort((a, b) => Number(b.state === "running") - Number(a.state === "running"));
  return (
    <Section eyebrow="Probe" title="Recent runs" count={jobs.length} bare>
      {jobs.length === 0 ? (
        <EmptyState>No probe run has reported yet.</EmptyState>
      ) : (
        <DataTable>
          <thead>
            <tr>
              <Th>Run</Th>
              <Th>Suite</Th>
              <Th>Region</Th>
              <Th>Started</Th>
              <Th numeric>Duration</Th>
              <Th numeric>Steps</Th>
              <Th>State</Th>
              <Th>Failed step</Th>
            </tr>
          </thead>
          <tbody>
            {jobs.map((j) => (
              <JobRows
                key={j.key}
                job={j}
                open={open.includes(j.key)}
                toggle={() => setOpen((o) => (o.includes(j.key) ? o.filter((k) => k !== j.key) : [...o, j.key]))}
              />
            ))}
          </tbody>
        </DataTable>
      )}
    </Section>
  );
}

function RunRow({ run, inset }: { run: SloRun; inset?: boolean }) {
  return (
    <Tr>
      <Td>
        <Glyph state={runGlyph(run.state)} className={`mr-2 inline-block align-middle ${inset ? "ml-6" : ""}`} />
        <Link
          href={`/superadmin/slo/runs/${encodeURIComponent(run.run_id)}`}
          className="font-mono text-caption text-primary underline-offset-4 hover:underline"
        >
          {run.run_id}
        </Link>
      </Td>
      <Td>{run.suite}</Td>
      <Td className="text-muted-foreground">{run.region}</Td>
      <Td className="text-muted-foreground">{when(new Date(run.started).getTime())}</Td>
      <Td numeric>{msLabel(run.duration_ms)}</Td>
      <Td numeric>{run.steps_failed > 0 ? `${run.steps_failed} of ${run.steps_total} failed` : run.steps_total}</Td>
      <Td>
        <Pill tone={runTone(run.state)}>{runStateLabel(run.state)}</Pill>
      </Td>
      <Td className="max-w-0 truncate text-muted-foreground">
        {run.failed_step ? `${run.failed_step} — ${run.failed_detail}` : "—"}
      </Td>
    </Tr>
  );
}

function JobRows({ job, open, toggle }: { job: SloJob; open: boolean; toggle: () => void }) {
  if (job.runs.length === 1) return <RunRow run={job.runs[0]} />;
  const failed = job.runs.find((r) => r.failed_step);
  return (
    <>
      <Tr>
        <Td>
          <button
            type="button"
            onClick={toggle}
            aria-expanded={open}
            className="flex items-center gap-2 text-left hover:underline underline-offset-4"
          >
            <span className="text-caption text-muted-foreground">{open ? "▾" : "▸"}</span>
            {job.runs.map((r) => (
              <span key={r.run_id} className="flex items-center gap-1">
                <span className="font-mono text-caption text-muted-foreground">g{r.group ?? 0}</span>
                <Glyph state={runGlyph(r.state)} />
              </span>
            ))}
          </button>
        </Td>
        <Td>{job.suite}</Td>
        <Td className="text-muted-foreground">{job.region}</Td>
        <Td className="text-muted-foreground">{when(new Date(job.started).getTime())}</Td>
        {/* The job took as long as its slowest group: the four run in parallel. */}
        <Td numeric>{msLabel(Math.max(...job.runs.map((r) => r.duration_ms)))}</Td>
        <Td numeric>
          {job.runs.reduce((a, r) => a + r.steps_failed, 0) > 0
            ? `${job.runs.reduce((a, r) => a + r.steps_failed, 0)} of ${job.runs.reduce((a, r) => a + r.steps_total, 0)} failed`
            : job.runs.reduce((a, r) => a + r.steps_total, 0)}
        </Td>
        <Td>
          <Pill tone={runTone(job.state)}>{runStateLabel(job.state)}</Pill>
        </Td>
        <Td className="max-w-0 truncate text-muted-foreground">
          {failed ? `${failed.failed_step} — ${failed.failed_detail}` : "—"}
        </Td>
      </Tr>
      {open && job.runs.map((r) => <RunRow key={r.run_id} run={r} inset />)}
    </>
  );
}
