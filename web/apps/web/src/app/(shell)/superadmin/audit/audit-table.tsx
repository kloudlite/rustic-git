"use client";

import { useState, useTransition } from "react";
import { auditQueryString, type AuditFilter, type AuditPage } from "@/lib/audit";
import { when } from "@/lib/time";
import { initials } from "@/lib/console";
import { resultPill } from "@/lib/audit-result";
import { loadMoreAudit } from "../actions";
import { Section } from "../ui/section";
import { DataTable, EmptyState, Td, Th, Tr } from "../ui/data-table";
import { Pill } from "../ui/pill";

/** `filters` is the server-rendered filter form, handed in as a slot so the section header holds
 *  both it and Export CSV while the row count stays client state (Load more grows it). */
export function AuditTable({
  initialPage,
  filter,
  filters,
}: {
  initialPage: AuditPage;
  filter: AuditFilter;
  filters: React.ReactNode;
}) {
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
    <Section
      eyebrow="History"
      title="Audit log"
      count={rows.length}
      bare
      toolbar={
        <>
          {filters}
          {/* Same-origin route handler, not the admin service directly — it holds the bearer
              token server-side and answers with a Content-Disposition download. */}
          <a
            href={`/superadmin/audit/export${auditQueryString(filter)}`}
            className="inline-flex h-8 items-center border border-border px-3 text-sm2 hover:bg-muted"
          >
            Export CSV
          </a>
        </>
      }
    >
      {error && <p className="px-4 py-2 text-caption text-destructive">{error}</p>}

      {rows.length === 0 ? (
        <EmptyState>Nothing matches that. Clear the filters to see every action again.</EmptyState>
      ) : (
        <DataTable>
          <thead>
            <tr>
              <Th numeric>When</Th>
              <Th>Actor</Th>
              <Th>Action</Th>
              <Th>Target</Th>
              <Th>Note</Th>
              <Th>Result</Th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r, i) => {
              const result = resultPill(r);
              return (
                <Tr key={`${r.ts}-${i}`}>
                  <Td numeric className="text-muted-foreground">{when(new Date(r.ts).getTime())}</Td>
                  <Td>
                    <span className="flex items-center gap-2">
                      <span className="flex size-5 shrink-0 items-center justify-center bg-muted text-micro font-medium text-muted-foreground">
                        {initials(r.actor)}
                      </span>
                      {r.actor}
                    </span>
                  </Td>
                  <Td><Pill>{r.action}</Pill></Td>
                  <Td className="font-mono text-caption">{r.target}</Td>
                  <Td className="text-muted-foreground">{r.reason ?? ""}</Td>
                  <Td><Pill tone={result.tone}>{result.label}</Pill></Td>
                </Tr>
              );
            })}
          </tbody>
        </DataTable>
      )}

      {cursor && (
        <div className="border-t border-border px-4 py-2">
          <button
            type="button"
            onClick={loadMore}
            disabled={pending}
            className="h-8 border border-border px-3 text-sm2 hover:bg-muted disabled:opacity-50"
          >
            {pending ? "Loading…" : "Load more"}
          </button>
        </div>
      )}
    </Section>
  );
}
