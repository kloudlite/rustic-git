"use client";

import { useMemo, useState } from "react";
import Link from "next/link";
import { Search } from "lucide-react";
import { Input } from "@/components/ui/input";
import type { OwnerRow, QuotaRequestDoc } from "@/lib/api";
import { dimLabel, dimUnit } from "@/lib/quota";
import { byTightest, tightest } from "@/lib/owners-sort";
import { Section } from "../ui/section";
import { CapacityBar } from "../ui/capacity-bar";
import { Pill } from "../ui/pill";
import { DataTable, EmptyState, RowActions, Td, Th, Tr } from "../ui/data-table";

type Kind = "all" | "team" | "person";

/** Same idiom as `repo-list.tsx`: the whole (small) fleet is already fetched, so filtering it
 *  locally is both simpler and faster than a round trip. The order is `byTightest` and nothing
 *  else — the one question this table answers is who hits a wall next. */
export function OwnersTable({ rows, pending }: { rows: OwnerRow[]; pending: QuotaRequestDoc[] }) {
  const [q, setQ] = useState("");
  const [kind, setKind] = useState<Kind>("all");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    return byTightest(
      rows.filter(
        (r) =>
          (!needle || r.owner.toLowerCase().includes(needle)) &&
          (kind === "all" || (kind === "team") === r.isTeam),
      ),
    );
  }, [rows, q, kind]);

  return (
    <Section
      eyebrow="Directory"
      title="Owners"
      count={rows.length}
      bare
      toolbar={
        <>
          <div className="relative w-56">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="Filter owners"
              aria-label="Filter owners"
              className="h-8 pl-8 text-sm2"
            />
          </div>
          <select
            value={kind}
            onChange={(e) => setKind(e.target.value as Kind)}
            aria-label="Kind"
            className="h-8 border border-border bg-card px-2 text-sm2"
          >
            <option value="all">All kinds</option>
            <option value="person">People</option>
            <option value="team">Teams</option>
          </select>
        </>
      }
    >
      {shown.length === 0 ? (
        <EmptyState>Nothing matches that — clear the filter to see every owner.</EmptyState>
      ) : (
        <DataTable>
          <thead>
            <tr>
              <Th>Owner</Th>
              <Th>Kind</Th>
              <Th>Tightest dimension</Th>
              <Th numeric>WS</Th>
              <Th numeric>Env</Th>
              <Th numeric>Snap</Th>
              <Th numeric>Disk</Th>
              <Th numeric>CPU</Th>
              <Th numeric>Mem</Th>
              <Th>Requests</Th>
              <Th />
            </tr>
          </thead>
          <tbody>
            {shown.map((r) => {
              const t = tightest(r);
              const waiting = pending.filter((p) => p.owner === r.owner).length;
              const href = `/superadmin/owners/${encodeURIComponent(r.owner)}`;
              return (
                <Tr key={r.owner}>
                  <Td>
                    <Link href={href} className="font-medium hover:underline">{r.owner}</Link>
                  </Td>
                  <Td><Pill>{r.isTeam ? "team" : "person"}</Pill></Td>
                  <Td className="w-56 py-2">
                    <p className="text-caption text-muted-foreground">{dimLabel(t.dim).toLowerCase()}</p>
                    <CapacityBar used={t.used} limit={t.limit} unit={dimUnit(t.dim)} />
                  </Td>
                  <Td numeric>{r.used.workspaces}</Td>
                  <Td numeric>{r.used.environments}</Td>
                  <Td numeric>{r.used.snapshots}</Td>
                  <Td numeric>{r.used.diskGb} GB</Td>
                  <Td numeric>{r.used.cpu}</Td>
                  <Td numeric>{r.used.memoryGb} GB</Td>
                  <Td>
                    {waiting > 0 ? (
                      <Pill tone="warn">{waiting} pending</Pill>
                    ) : (
                      <span className="text-muted-foreground">—</span>
                    )}
                  </Td>
                  <Td>
                    <RowActions>
                      <Link href={href} className="text-sm2 text-muted-foreground hover:text-primary">Set quota</Link>
                      <Link href={`/${encodeURIComponent(r.owner)}/workspaces`} className="text-sm2 text-muted-foreground hover:text-primary">
                        Open as
                      </Link>
                    </RowActions>
                  </Td>
                </Tr>
              );
            })}
          </tbody>
        </DataTable>
      )}
      <p className="border-t border-border px-4 py-2 text-caption text-muted-foreground">
        {shown.length} of {rows.length} owners · sorted by tightest dimension
      </p>
    </Section>
  );
}
