"use client";

import { useState, useTransition } from "react";
import { Loader2 } from "lucide-react";
import Link from "next/link";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { when, stamp } from "@/lib/time";
import { deleteNowConfirm, removalKey } from "@/lib/team-removal";
import type { ApiRemoval } from "@/lib/api";
import { deleteRemovalNowAction } from "../actions";
import { Section } from "../ui/section";
import { DataTable, EmptyState, Td, Th, Tr } from "../ui/data-table";

type Row = ApiRemoval & { team: string };

/** Delete-now asks for the handle typed out, same shape as the team's own settings page
 *  (`components/app/team-settings.tsx`'s `RemovalRow`) — irreversible, so the confirm is a value
 *  typed rather than a click, and it names both the person and the team out loud
 *  (`deleteNowConfirm`, shared with that page) so a handle pending in two teams is never
 *  ambiguous about which one this row acts on. One row opens its confirm at a time.*/
function RemovalRow({ r, open, onOpen, onClose }: { r: Row; open: boolean; onOpen: () => void; onClose: () => void }) {
  const [typed, setTyped] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();
  const at = Date.parse(r.delete_at);

  function submit() {
    if (typed !== r.owner) return;
    startTransition(async () => {
      const res = await deleteRemovalNowAction(r.team, r.owner, typed);
      if (!res.ok) {
        setError(res.message);
        return;
      }
      setError(null);
      setNotice(res.notice);
    });
  }

  return (
    <Tr>
      <Td>
        <span className="font-mono">@{r.owner}</span>
      </Td>
      <Td>
        <Link href={`/superadmin/owners/${encodeURIComponent(r.team)}`} className="hover:underline">
          {r.team}
        </Link>
      </Td>
      <Td>
        <span title={stamp(at)}>{when(at)}</span>
      </Td>
      <Td>
        {notice ? (
          <span className="text-caption text-muted-foreground">{notice}</span>
        ) : open ? (
          <div className="flex flex-wrap items-center gap-2">
            <p className="w-full text-caption text-muted-foreground">{deleteNowConfirm(r.owner, r.team)}</p>
            <Input
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              autoComplete="off"
              placeholder={r.owner}
              aria-label={deleteNowConfirm(r.owner, r.team)}
              className="h-8 max-w-40 font-mono text-sm2"
            />
            <Button type="button" variant="destructive" size="sm" disabled={pending || typed !== r.owner} onClick={submit}>
              {pending && <Loader2 className="animate-spin" />}Delete now
            </Button>
            <Button type="button" variant="ghost" size="sm" onClick={onClose}>Cancel</Button>
            {error && <p role="alert" className="w-full text-caption font-medium text-destructive">{error}</p>}
          </div>
        ) : (
          <Button type="button" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" onClick={onOpen}>
            Delete now
          </Button>
        )}
      </Td>
    </Tr>
  );
}

/** `GET /admin/owners/removals`: every removed member across every team, still waiting out the
 *  grace period. Delete-now is the one write this table offers — the listing itself is read-only,
 *  same as the rest of the Owners screen. */
export function PendingRemovals({ rows }: { rows: Row[] }) {
  const [openKey, setOpenKey] = useState<string | null>(null);
  return (
    <Section eyebrow="Directory" title="Pending removals" count={rows.length} bare>
      {rows.length === 0 ? (
        <EmptyState>Nobody is waiting on a removal right now.</EmptyState>
      ) : (
        <DataTable>
          <thead>
            <tr>
              <Th>Owner</Th>
              <Th>Team</Th>
              <Th>Data deleted</Th>
              <Th />
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => {
              const key = removalKey(r.team, r.owner);
              return (
                <RemovalRow
                  key={key}
                  r={r}
                  open={openKey === key}
                  onOpen={() => setOpenKey(key)}
                  onClose={() => setOpenKey(null)}
                />
              );
            })}
          </tbody>
        </DataTable>
      )}
    </Section>
  );
}
