import type { SloStatus } from "@/lib/api";
import { budgetLabel, budgetSpentPct, burnLabel, groupByFeature, sloTone, windowLabel } from "@/lib/slo";
import { when } from "@/lib/time";
import { Section } from "../ui/section";
import { CapacityBar } from "../ui/capacity-bar";
import { DataTable, Td, Th, Tr, EmptyState } from "../ui/data-table";
import { Pill } from "../ui/pill";

/** The catalogue (`deploy/slo.md`) as the probe has measured it — one row per SLO, grouped by
 *  feature in the catalogue's own order with a feature header row between groups.
 *
 *  The budget bar is spent-of-budget, not attainment: 100 % attainment and "0 % of the budget
 *  spent" are the same fact, but only the second is a bar an operator can read at a glance next
 *  to a target of 99.9 %, where the whole story lives in the last tenth.
 *
 *  ponytail: the spec's "row click expands the last ten samples" is not rendered — `SloStatus`
 *  carries only `last`, and there is no endpoint for a sample history. Upgrade path: a
 *  `GET /admin/slo/{id}/samples` and a client row wrapper here. */
export function SloTable({ slos }: { slos: SloStatus[] }) {
  const groups = groupByFeature(slos);
  return (
    <Section eyebrow="Catalogue" title="Service level objectives" count={slos.length} bare>
      {slos.length === 0 ? (
        <EmptyState>No SLO has been measured yet. The first probe run seeds this table.</EmptyState>
      ) : (
        <DataTable>
          <thead>
            <tr>
              <Th>SLI</Th>
              <Th>Target</Th>
              <Th numeric>30 d</Th>
              <Th className="w-48">Error budget</Th>
              <Th numeric>Burn (short)</Th>
              <Th numeric>Burn (long)</Th>
              <Th>Last result</Th>
              <Th>State</Th>
            </tr>
          </thead>
          <tbody>
            {groups.map((g) => (
              <Fragmentish key={g.feature} feature={g.feature} slos={g.slos} />
            ))}
          </tbody>
        </DataTable>
      )}
    </Section>
  );
}

/** A feature's header row and its SLOs. Its own component only because a `<tbody>` may not hold a
 *  fragment with a key in a way that keeps the rows siblings of the other groups' rows. */
function Fragmentish({ feature, slos }: { feature: string; slos: SloStatus[] }) {
  return (
    <>
      <tr>
        <th
          scope="colgroup"
          colSpan={8}
          className="border-b border-border bg-muted/50 px-3 py-1.5 text-left text-micro font-medium tracking-eyebrow text-muted-foreground uppercase"
        >
          {feature}
        </th>
      </tr>
      {slos.map((s) => (
        <Tr key={s.id}>
          <Td className="font-medium">
            {s.sli}
            <span className="ml-2 font-mono text-caption text-muted-foreground">{s.id}</span>
          </Td>
          <Td className="tabular-nums text-muted-foreground">{s.target}</Td>
          <Td numeric className="whitespace-nowrap">{s.attainment_30d == null ? "—" : `${(s.attainment_30d * 100).toFixed(2)} %`}</Td>
          <Td>
            {s.budget_left == null ? (
              <span className="text-caption text-muted-foreground">no samples in 30 d</span>
            ) : (
              // Spent, clamped at the wall: a breached budget is 100 % of a bar, and the label
              // beside it is what says by how much it was overspent.
              <CapacityBar
                used={budgetSpentPct(s.budget_left, s.budget_30d)}
                limit={100}
                unit="%"
                label={`${budgetLabel(s.budget_left, s.budget_30d)} · ${s.total_30d} samples`}
                quiet
                className="whitespace-nowrap"
              />
            )}
          </Td>
          {/* Each row names its own window: the catalogue mixes per-request SLOs with weekly
              ones, so a single "burn 1 h / 6 h" header would mislabel half the table. */}
          <Td numeric>
            {burnLabel(s.burn_short)}
            <span className="ml-1.5 text-caption text-muted-foreground">{windowLabel(s.window_short_secs)}</span>
          </Td>
          <Td numeric>
            {burnLabel(s.burn_long)}
            <span className="ml-1.5 text-caption text-muted-foreground">{windowLabel(s.window_long_secs)}</span>
          </Td>
          <Td className="text-muted-foreground">
            {s.last ? `${s.last.ok ? "ok" : "failed"} · ${s.last.ms} ms · ${when(new Date(s.last.ts).getTime())}` : "never run"}
          </Td>
          <Td>
            <Pill tone={sloTone(s.state)}>{s.state}</Pill>
          </Td>
        </Tr>
      ))}
    </>
  );
}
