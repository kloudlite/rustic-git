import type { SignalsResponse } from "@/lib/api";
import { Section } from "./ui/section";
import { DataTable, Td, Th, Tr } from "./ui/data-table";
import { Pill } from "./ui/pill";

/** The alert catalogue (`deploy/alerts.md`), scraped and evaluated on the request path —
 *  `crates/workspaces/src/api/admin/monitoring.rs::signal_rows`. One row per rule: its state,
 *  the catalogue's own reason it exists, and what this evaluation actually observed (or why it
 *  could not).
 *
 *  ponytail: the mockup's "Last change" column is not rendered — the wire `SignalRow` carries no
 *  such field (spec §A3's `kloudlite.alerts` table is not deployed), and a column of "—" invents
 *  nothing but reads as a broken one. Upgrade path: add `lastChange` to the Rust `SignalRow` and
 *  a cell below. Rows sort by region (fleet-wide rules, `region: null`, last) so the table reads
 *  as grouped without a second fetch or a sub-header component. */
export function SignalsTable({ data }: { data: SignalsResponse }) {
  const rows = [...data.signals].sort((a, b) => (a.region ?? "￿").localeCompare(b.region ?? "￿"));
  return (
    <Section
      eyebrow="Signals"
      title="Alert rules"
      count={data.signals.length}
      bare
      toolbar={
        data.hyperdx_url && (
          <a
            href={data.hyperdx_url}
            target="_blank"
            rel="noreferrer"
            className="inline-flex h-8 items-center border border-border px-3 text-sm2 hover:bg-muted"
          >
            Open in HyperDX
          </a>
        )
      }
    >
      <DataTable>
        <thead>
          <tr>
            <Th>Rule</Th>
            <Th>Region</Th>
            <Th>State</Th>
            <Th>Why it fires</Th>
            <Th>Detail</Th>
          </tr>
        </thead>
        <tbody>
          {rows.map((s) => (
            <Tr key={s.alert + (s.region ?? "")}>
              <Td className="font-medium">{s.alert}</Td>
              <Td className="text-muted-foreground">{s.region ?? "fleet"}</Td>
              <Td>
                <Pill tone={s.state === "firing" ? "critical" : s.state === "ok" ? "ok" : "neutral"}>{s.state}</Pill>
              </Td>
              <Td className="text-muted-foreground">{s.why}</Td>
              <Td className="font-mono text-caption tabular-nums text-muted-foreground">{s.detail ?? "—"}</Td>
            </Tr>
          ))}
        </tbody>
      </DataTable>
    </Section>
  );
}
