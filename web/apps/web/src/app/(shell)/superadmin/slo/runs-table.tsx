import Link from "next/link";
import type { SloRun } from "@/lib/api";
import { when } from "@/lib/time";
import { Section } from "../ui/section";
import { DataTable, Td, Th, Tr, EmptyState } from "../ui/data-table";
import { Pill } from "../ui/pill";
import { msLabel, runTone } from "@/lib/slo";
import { Glyph } from "./run-tree";

/** The last twenty probe runs. The failure detail is truncated to one line on purpose — it is a
 *  human-written sentence and can be two thousand characters; the run page shows all of it. */
export function RunsTable({ runs }: { runs: SloRun[] }) {
  // The run in flight first, whatever order the api answered in: it is the only row still
  // changing, and a reader scanning for it should never have to look past the first line.
  const rows = [...runs].sort((a, b) => Number(b.state === "running") - Number(a.state === "running"));
  return (
    <Section eyebrow="Probe" title="Recent runs" count={runs.length} bare>
      {runs.length === 0 ? (
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
            {rows.map((r) => (
              <Tr key={r.run_id}>
                <Td>
                  <Glyph state={r.state === "running" ? "running" : r.state === "failed" ? "failed" : "passed"} className="mr-2 inline-block align-middle" />
                  <Link
                    href={`/superadmin/slo/runs/${encodeURIComponent(r.run_id)}`}
                    className="font-mono text-caption text-primary underline-offset-4 hover:underline"
                  >
                    {r.run_id}
                  </Link>
                </Td>
                <Td>{r.suite}</Td>
                <Td className="text-muted-foreground">{r.region}</Td>
                <Td className="text-muted-foreground">{when(new Date(r.started).getTime())}</Td>
                <Td numeric>{msLabel(r.duration_ms)}</Td>
                <Td numeric>
                  {r.steps_failed > 0 ? `${r.steps_failed} of ${r.steps_total} failed` : r.steps_total}
                </Td>
                <Td>
                  <Pill tone={runTone(r.state)}>{r.state}</Pill>
                </Td>
                <Td className="max-w-0 truncate text-muted-foreground">
                  {r.failed_step ? `${r.failed_step} — ${r.failed_detail}` : "—"}
                </Td>
              </Tr>
            ))}
          </tbody>
        </DataTable>
      )}
    </Section>
  );
}
