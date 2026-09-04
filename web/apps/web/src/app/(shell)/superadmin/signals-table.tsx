import type { SignalsResponse } from "@/lib/api";
import { Section } from "./ui/section";
import { DataTable, Td, Th, Tr } from "./ui/data-table";
import { Pill } from "./ui/pill";

/** The alert catalogue (`deploy/alerts.md`), scraped and evaluated on the request path —
 *  `crates/workspaces/src/api/admin/monitoring.rs::signal_rows`. One row per rule: its state,
 *  the catalogue's own reason it exists, and what this evaluation actually observed (or why it
 *  could not).
 *
 *  ponytail: the mockup's "Last change" column and "source" chip are not rendered — the wire
 *  `SignalRow` carries neither (spec §A3's `rustic.alerts` table is not deployed), and a column
 *  of "—" invents nothing but reads as a broken one. Upgrade path: add `lastChange`/`source` to
 *  the Rust `SignalRow` and the two cells below. The same applies to the mockup's HyperDX button:
 *  `SignalsResponse` carries only `grafana_url`, so only that link renders. */
export function SignalsTable({ data }: { data: SignalsResponse }) {
  return (
    <Section
      eyebrow="Signals"
      title="Alert rules"
      count={data.signals.length}
      bare
      toolbar={
        data.grafana_url && (
          <a
            href={data.grafana_url}
            target="_blank"
            rel="noreferrer"
            className="inline-flex h-8 items-center border border-border px-3 text-sm2 hover:bg-muted"
          >
            Grafana
          </a>
        )
      }
    >
      <DataTable>
        <thead>
          <tr>
            <Th>Rule</Th>
            <Th>State</Th>
            <Th>Why it fires</Th>
            <Th>Detail</Th>
          </tr>
        </thead>
        <tbody>
          {data.signals.map((s) => (
            <Tr key={s.alert}>
              <Td className="font-medium">{s.alert}</Td>
              <Td>
                <Pill tone={s.state === "firing" ? "critical" : s.state === "ok" ? "ok" : "neutral"}>{s.state}</Pill>
              </Td>
              <Td className="text-muted-foreground">{s.why}</Td>
              <Td className="font-mono text-caption text-muted-foreground">{s.detail ?? "—"}</Td>
            </Tr>
          ))}
        </tbody>
      </DataTable>
    </Section>
  );
}
