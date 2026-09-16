"use client";

import { useState, useTransition } from "react";
import { useRouter } from "next/navigation";
import { Loader2 } from "lucide-react";
import type { SloExclusion, SloStatus } from "@/lib/api";
import { exclusionPayload } from "@/lib/slo";
import { ist, istInput } from "@/lib/time";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import { Section } from "../ui/section";
import { DataTable, Td, Th, Tr, EmptyState, RowActions } from "../ui/data-table";
import { excludeWindowAction, unexcludeWindowAction } from "../actions";

/** Acknowledged incident windows: the ones in force, and the form that adds one.
 *
 *  An exclusion moves numbers other people decide from, so it is a write like any other loud one —
 *  a required reason, the api's own refusal shown rather than swallowed, and an audit row on the
 *  admin side. It never deletes a sample: the raw counts stay in `slo_results` and the SLO table
 *  above says how many samples each window took out. */
export function Exclusions({ exclusions, slos }: { exclusions: SloExclusion[]; slos: SloStatus[] }) {
  const router = useRouter();
  const [removing, setRemoving] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  return (
    <Section
      eyebrow="Budgets"
      title="Excluded windows"
      count={exclusions.length}
      bare
      toolbar={<ExcludeDialog slos={slos} label="Exclude a window…" />}
    >
      {error && <p role="alert" className="border-b border-border px-4 py-2 text-sm2 font-medium text-destructive">{error}</p>}
      {exclusions.length === 0 ? (
        <EmptyState>No window is excluded. Every sample counts towards every budget.</EmptyState>
      ) : (
        <DataTable>
          <thead>
            <tr>
              <Th>Window (IST)</Th>
              <Th>SLOs</Th>
              <Th>Why</Th>
              <Th>Who</Th>
              <Th>When</Th>
              <Th />
            </tr>
          </thead>
          <tbody>
            {exclusions.map((x) => (
              <Tr key={x.id}>
                <Td className="whitespace-nowrap font-medium">
                  {ist(new Date(x.from).getTime())} → {ist(new Date(x.to).getTime())}
                </Td>
                <Td className="text-muted-foreground">
                  {x.slo_ids.length === 0 ? "all" : <span className="font-mono text-caption">{x.slo_ids.join(", ")}</span>}
                </Td>
                <Td>{x.note}</Td>
                <Td className="text-muted-foreground">{x.by}</Td>
                <Td className="whitespace-nowrap text-muted-foreground">{ist(new Date(x.created).getTime())}</Td>
                <Td>
                  <RowActions>
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    disabled={pending && removing === x.id}
                    onClick={() => {
                      setRemoving(x.id);
                      setError(null);
                      startTransition(async () => {
                        const r = await unexcludeWindowAction(x.id);
                        if (!r.ok) setError(r.message);
                        else router.refresh();
                      });
                    }}
                  >
                    {pending && removing === x.id && <Loader2 className="animate-spin" />}
                    Remove
                  </Button>
                  </RowActions>
                </Td>
              </Tr>
            ))}
          </tbody>
        </DataTable>
      )}
    </Section>
  );
}

/** The button and its form. `from`/`to` default to the window it was opened over (a failed run's
 *  own start and finish, from the run page) and `sloIds` to that run's failed ids; with nothing
 *  selected the exclusion covers every SLO, which is what an infrastructure incident is. */
export function ExcludeDialog({
  slos,
  label,
  from,
  to,
  sloIds = [],
}: {
  slos: SloStatus[];
  label: string;
  from?: string;
  to?: string;
  sloIds?: string[];
}) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [fromAt, setFromAt] = useState("");
  const [toAt, setToAt] = useState("");
  const [ids, setIds] = useState<string[]>([]);
  const [note, setNote] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  function submit() {
    setError(null);
    const shaped = exclusionPayload({ from: fromAt, to: toAt, sloIds: ids, note });
    if (!shaped.ok) {
      setError(shaped.message);
      return;
    }
    startTransition(async () => {
      const r = await excludeWindowAction(shaped.body);
      if (!r.ok) {
        setError(r.message);
        return;
      }
      setOpen(false);
      router.refresh();
    });
  }

  return (
    <>
      <Button
        type="button"
        size="sm"
        variant="outline"
        onClick={() => {
          const now = Date.now();
          setFromAt(from ? istInput(new Date(from).getTime()) : istInput(now - 3_600_000));
          setToAt(to ? istInput(new Date(to).getTime()) : istInput(now));
          setIds(sloIds.filter((id) => slos.some((s) => s.id === id)));
          setNote("");
          setError(null);
          setOpen(true);
        }}
      >
        {label}
      </Button>
      <Dialog open={open} onOpenChange={(o) => !o && setOpen(false)}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>Exclude a window</DialogTitle>
            <DialogDescription>
              The samples in this window stop counting towards the budgets. They are not deleted, and the
              SLO table states how many each window took out. A window may cover at most seven days.
            </DialogDescription>
          </DialogHeader>
          <div className="grid gap-3">
            <div className="grid grid-cols-2 gap-3">
              <label className="grid gap-1 text-sm2">
                From (IST)
                <Input type="datetime-local" value={fromAt} onChange={(e) => setFromAt(e.target.value)} className="h-8" />
              </label>
              <label className="grid gap-1 text-sm2">
                To (IST)
                <Input type="datetime-local" value={toAt} onChange={(e) => setToAt(e.target.value)} className="h-8" />
              </label>
            </div>
            <label className="grid gap-1 text-sm2">
              SLOs — select none for all
              <select
                multiple
                size={6}
                value={ids}
                onChange={(e) => setIds([...e.target.selectedOptions].map((o) => o.value))}
                className="border border-input bg-background px-2 py-1 font-mono text-caption"
              >
                {slos.map((s) => (
                  <option key={s.id} value={s.id}>
                    {s.id}
                  </option>
                ))}
              </select>
            </label>
            <label className="grid gap-1 text-sm2">
              Why (required)
              <Textarea value={note} onChange={(e) => setNote(e.target.value)} rows={3} />
            </label>
          </div>
          {error && <p role="alert" className="text-sm2 font-medium text-destructive">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="button" onClick={submit} disabled={pending || note.trim() === ""}>
              {pending && <Loader2 className="animate-spin" />}
              Exclude
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
