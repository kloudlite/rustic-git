import type { SignalsResponse } from "@/lib/api";
import { SignalBadge } from "./status-badge";

/** The alert catalogue (`deploy/alerts.md`), scraped and evaluated on the request path —
 *  `crates/workspaces/src/api/admin/monitoring.rs::signal_rows`. One row per rule: its state,
 *  the catalogue's own reason it exists, and what this evaluation actually observed (or why it
 *  could not). Restarts and the Grafana link ride along on the same response rather than a
 *  second round trip. */
export function SignalsTable({ data }: { data: SignalsResponse }) {
  return (
    <div className="space-y-4">
      <div className="overflow-x-auto border border-border bg-card">
        <div className="flex items-center justify-between px-3 py-2 border-b border-border">
          <h2 className="text-sm2 font-medium">Signals</h2>
          {data.grafana_url && (
            <a href={data.grafana_url} target="_blank" rel="noreferrer" className="text-caption text-primary hover:underline">
              Grafana
            </a>
          )}
        </div>
        <table className="w-full text-sm2">
          <thead className="border-b border-border text-left text-caption text-muted-foreground">
            <tr>
              <th className="px-3 py-2 font-medium">Rule</th>
              <th className="px-3 py-2 font-medium">State</th>
              <th className="px-3 py-2 font-medium">Why it matters</th>
              <th className="px-3 py-2 font-medium">Detail</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {data.signals.map((s) => (
              <tr key={s.alert}>
                <td className="px-3 py-2 align-top font-mono text-caption">{s.alert}</td>
                <td className="px-3 py-2 align-top"><SignalBadge state={s.state} /></td>
                <td className="px-3 py-2 align-top text-caption text-muted-foreground">{s.why}</td>
                <td className="px-3 py-2 align-top text-caption text-muted-foreground tabular-nums">{s.detail ?? "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {data.restarts.length > 0 && (
        <div className="border border-border bg-card px-3 py-2">
          <h2 className="mb-2 text-sm2 font-medium">Restarts (since pod start)</h2>
          <dl className="flex flex-wrap gap-x-6 gap-y-1 text-caption">
            {data.restarts.map((r) => (
              <div key={r.workload} className="flex items-baseline gap-1.5">
                <dt className="font-mono text-muted-foreground">{r.workload}</dt>
                <dd className="tabular-nums font-medium">{r.restarts}</dd>
              </div>
            ))}
          </dl>
        </div>
      )}
    </div>
  );
}
