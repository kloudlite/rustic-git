"use client";

import { useState, useTransition } from "react";
import { auditQueryString, type AuditFilter, type AuditPage } from "@/lib/audit";
import { when } from "@/lib/time";
import { loadMoreAudit } from "../actions";

export function AuditTable({ initialPage, filter }: { initialPage: AuditPage; filter: AuditFilter }) {
  const [rows, setRows] = useState(initialPage.rows);
  const [cursor, setCursor] = useState(initialPage.next_cursor ?? null);
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  function loadMore() {
    if (!cursor) return;
    startTransition(async () => {
      const r = await loadMoreAudit(filter, cursor);
      if (!r.ok) {
        setError(r.message);
        return;
      }
      setRows((prev) => [...prev, ...r.page.rows]);
      setCursor(r.page.next_cursor ?? null);
    });
  }

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <span className="text-caption text-muted-foreground">{rows.length} row{rows.length === 1 ? "" : "s"}</span>
        {/* Same-origin route handler, not the admin service directly — it holds the bearer token
         *  server-side and answers with a Content-Disposition download. */}
        <a
          href={`/superadmin/audit/export${auditQueryString(filter)}`}
          className="h-8 border border-border px-3 text-sm2 leading-8 hover:bg-muted"
        >
          Export CSV
        </a>
      </div>

      {error && (
        <p className="border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 text-destructive">{error}</p>
      )}

      {rows.length === 0 ? (
        <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
          Nothing matches that.
        </p>
      ) : (
        <div className="overflow-x-auto border border-border bg-card">
          <table className="w-full text-sm2">
            <thead>
              <tr className="border-b border-border text-left text-caption text-muted-foreground">
                <th className="px-3 py-2 font-normal">When</th>
                <th className="px-3 py-2 font-normal">Actor</th>
                <th className="px-3 py-2 font-normal">Action</th>
                <th className="px-3 py-2 font-normal">Target</th>
                <th className="px-3 py-2 font-normal">Reason</th>
                <th className="px-3 py-2 font-normal">Result</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border">
              {rows.map((r, i) => (
                <tr key={`${r.ts}-${i}`}>
                  <td className="tabular-nums px-3 py-2 text-muted-foreground">{when(new Date(r.ts).getTime())}</td>
                  <td className="px-3 py-2">{r.actor}</td>
                  <td className="px-3 py-2">{r.action}</td>
                  <td className="px-3 py-2">{r.target}</td>
                  <td className="px-3 py-2 text-muted-foreground">{r.reason ?? ""}</td>
                  <td className={`px-3 py-2 ${r.result.startsWith("error") ? "text-destructive" : "text-muted-foreground"}`}>
                    {r.result}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {cursor && (
        <button
          type="button"
          onClick={loadMore}
          disabled={pending}
          className="h-8 self-start border border-border px-3 text-sm2 hover:bg-muted disabled:opacity-50"
        >
          {pending ? "Loading…" : "Load more"}
        </button>
      )}
    </div>
  );
}
