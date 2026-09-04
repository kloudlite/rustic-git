"use client";

import { useMemo, useState } from "react";
import Link from "next/link";
import { Search } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import type { OwnerRow } from "@/lib/api";
import { atLimit, DIMS, dimLabel, tightestRatio } from "@/lib/quota";

/** Same idiom as `repo-list.tsx`: the whole (small) fleet is already fetched, so filtering and
 *  sorting it locally is both simpler and faster than a round trip. */
export function OwnersTable({ rows }: { rows: OwnerRow[] }) {
  const [q, setQ] = useState("");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    const filtered = needle ? rows.filter((r) => r.owner.toLowerCase().includes(needle)) : rows;
    // Ascending: the row with the least headroom on any dimension (the smallest ratio) sorts
    // first — the operator's question is "who is closest to their limit".
    return [...filtered].sort((a, b) => tightestRatio(a.limit, a.used) - tightestRatio(b.limit, b.used));
  }, [rows, q]);

  const pendingCount = rows.filter((r) => r.pending).length;

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-3">
        <div className="relative w-full max-w-xs">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Find an owner"
            aria-label="Find an owner"
            className="h-8 pl-8 text-sm2"
          />
        </div>
        <span className="ml-auto text-caption text-muted-foreground">
          {rows.length} owners{pendingCount > 0 ? ` · ${pendingCount} with a pending request` : ""}
        </span>
      </div>

      {shown.length === 0 ? (
        <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
          Nothing matches that.
        </p>
      ) : (
        <div className="overflow-x-auto border border-border bg-card">
          <table className="w-full text-left text-sm2">
            <thead>
              <tr className="border-b border-border text-caption text-muted-foreground">
                <th className="px-4 py-2 font-medium">Owner</th>
                {DIMS.map((d) => (
                  <th key={d} className="px-4 py-2 font-medium">{dimLabel(d)}</th>
                ))}
                <th className="px-4 py-2 font-medium">Limits from</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((r) => (
                <tr key={r.owner} className="border-b border-border last:border-0 hover:bg-muted/50">
                  <td className="px-4 py-2">
                    <Link href={`/superadmin/owners/${encodeURIComponent(r.owner)}`} className="font-medium text-foreground">
                      {r.owner}
                    </Link>{" "}
                    <Badge variant="outline">{r.isTeam ? "team" : "person"}</Badge>
                    {r.pending && (
                      <Badge variant="outline" className="ml-1 border-warning/40 bg-warning/10 text-warning">
                        request
                      </Badge>
                    )}
                  </td>
                  {DIMS.map((d) => (
                    <td key={d} className="px-4 py-2 tabular-nums">
                      <span className={atLimit(r, d) ? "font-semibold text-destructive" : "text-muted-foreground"}>
                        {r.used[d]}
                      </span>
                      <span className="text-muted-foreground/60"> / {r.limit[d]}</span>
                    </td>
                  ))}
                  <td className="px-4 py-2 text-caption text-muted-foreground">
                    {r.source === "own" ? "own quota" : r.isTeam ? "team default" : "person default"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
