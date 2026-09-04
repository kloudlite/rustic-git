"use client";

import { useState, useTransition } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { changedFields, confirmationFor, type SettingRow } from "@/lib/settings";
import type { WorkloadDoc } from "@/lib/api";
import type { SaveResult } from "./actions";

/** One row per field, editable, with its env/default/range/mark/last-change context — spec §5's
 *  table. Shared by the central tab and every region's cluster panel: the row shape (name,
 *  description, default, env, stored, range, last-change, pending) is identical across scopes, so
 *  there is one component parameterized by `onSave` rather than two near-duplicates. */
export function SettingsTable({
  rows,
  workloads,
  onSave,
  onRevert,
}: {
  rows: SettingRow[];
  /** Freshest `GET /admin/workloads` snapshot for THIS scope — refetched by the server component
   *  on every `router.refresh()` tick, same as `rows` itself. */
  workloads: WorkloadDoc[];
  onSave: (patch: Record<string, unknown>) => Promise<SaveResult>;
  /** Cluster scope only — central has no revert route yet (see `actions.ts`). */
  onRevert?: () => Promise<SaveResult>;
}) {
  const [edited, setEdited] = useState<Record<string, unknown>>({});
  // Set right after a successful save: the values that were POSTed. NOT shrunk by an effect —
  // `pending`/`rollingReaders` below are DERIVED from this plus the latest `rows`/`workloads`
  // props on every render, which is what "poll every few seconds ... clear it once the polled
  // value matches" (spec §5) reduces to once the poll itself is just `router.refresh()` handing
  // this component fresh props (`AutoRefresh`, mounted below while anything is still pending).
  const [savedPatch, setSavedPatch] = useState<Record<string, unknown>>({});
  const [savedReaders, setSavedReaders] = useState<string[]>([]);
  const [confirmOpen, setConfirmOpen] = useState<
    null | { message: string; needsSecond: boolean; onSecond: boolean; patch: Record<string, unknown> }
  >(null);
  const [error, setError] = useState<string | null>(null);
  const [pendingSave, startSave] = useTransition();

  const pending = Object.fromEntries(
    Object.entries(savedPatch).filter(([key, value]) => rows.find((r) => r.key === key)?.value !== value),
  );
  const rollingReaders = savedReaders.filter((name) => {
    const w = workloads.find((wl) => wl.name === name);
    return !w || w.ready < w.desired;
  });

  function setValue(key: string, value: unknown) {
    setEdited((prev) => ({ ...prev, [key]: value }));
  }

  function requestSave() {
    setError(null);
    const patch = changedFields(rows, edited);
    if (Object.keys(patch).length === 0) return;
    const confirmation = confirmationFor(rows, Object.keys(patch));
    if (!confirmation.needsConfirm) {
      void doSave(patch);
      return;
    }
    // First confirmation always; the second (rustic-git-srv) is asked only after the first is
    // accepted — the dialog's own button flips `onSecond` rather than stacking two dialogs.
    setConfirmOpen({
      message: confirmation.message,
      needsSecond: confirmation.needsSecondConfirm,
      onSecond: false,
      patch,
    });
  }

  async function doSave(patch: Record<string, unknown>) {
    setConfirmOpen(null);
    startSave(async () => {
      const result = await onSave(patch);
      if (!result.ok) {
        setError(result.message);
        return;
      }
      setSavedPatch((prev) => ({ ...prev, ...patch }));
      const readers = confirmationFor(rows, Object.keys(patch));
      if (readers.needsConfirm) setSavedReaders((prev) => [...new Set([...prev, ...readers.readers])]);
      setEdited((prev) => {
        const next = { ...prev };
        for (const k of Object.keys(patch)) delete next[k];
        return next;
      });
    });
  }

  const isPending = Object.keys(pending).length > 0 || rollingReaders.length > 0;

  return (
    <div className="space-y-4">
      {isPending && <AutoRefresh intervalMs={3_000} />}
      <div className="overflow-x-auto border border-border bg-card">
        <table className="w-full text-sm2">
          <thead className="border-b border-border text-left text-caption text-muted-foreground">
            <tr>
              <th className="px-3 py-2 font-medium">Name</th>
              <th className="px-3 py-2 font-medium">Value</th>
              <th className="px-3 py-2 font-medium">Env / default</th>
              <th className="px-3 py-2 font-medium">Mark</th>
              <th className="px-3 py-2 font-medium">Last change</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {rows.map((row) => (
              <tr key={row.key}>
                <td className="px-3 py-2 align-top">
                  <div className="font-medium">{row.key}</div>
                  <div className="text-caption text-muted-foreground">{row.description}</div>
                  {row.range && (
                    <div className="text-caption text-muted-foreground">
                      {row.range.min}–{row.range.max} {row.unit}
                    </div>
                  )}
                </td>
                <td className="px-3 py-2 align-top">
                  <RowInput row={row} value={edited[row.key] ?? row.value} onChange={(v) => setValue(row.key, v)} />
                  {row.key in pending && (
                    <span aria-label="pending" title="Save requested; waiting for it to apply" className="ml-2 inline-block size-2 rounded-full bg-warning align-middle" />
                  )}
                </td>
                <td className="px-3 py-2 align-top text-caption text-muted-foreground">
                  <div>env: {String(row.envValue ?? "—")}</div>
                  <div>default: {String(row.defaultValue)}</div>
                </td>
                <td className="px-3 py-2 align-top text-caption">
                  {row.mark === "boot" ? `boot: ${row.readers.join(", ") || "—"}` : "live"}
                </td>
                <td className="px-3 py-2 align-top text-caption text-muted-foreground">
                  {row.lastChangedBy ? `${row.lastChangedBy} · ${row.lastChangedAt}` : "—"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {rollingReaders.length > 0 && (
        <ul className="text-caption text-muted-foreground">
          {rollingReaders.map((name) => {
            const w = workloads.find((wl) => wl.name === name);
            return (
              <li key={name}>
                {name}: {w ? `${w.ready}/${w.desired} ready` : "waiting for status"}
              </li>
            );
          })}
        </ul>
      )}

      {error && <p role="alert" className="text-sm2 font-medium text-destructive">{error}</p>}

      <div className="flex items-center gap-3">
        <Button onClick={requestSave} disabled={pendingSave || Object.keys(edited).length === 0}>
          {pendingSave && <Loader2 className="animate-spin" />}Save
        </Button>
        {onRevert && (
          <Button
            type="button"
            variant="outline"
            disabled={pendingSave}
            onClick={() => startSave(async () => {
              const r = await onRevert();
              if (!r.ok) setError(r.message);
            })}
          >
            Revert last change
          </Button>
        )}
      </div>

      <Dialog open={confirmOpen !== null} onOpenChange={(open) => !open && setConfirmOpen(null)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{confirmOpen?.onSecond ? "This rolls rustic-git-srv" : "Save and roll"}</DialogTitle>
            <DialogDescription>
              {confirmOpen?.onSecond
                ? "Rolling rustic-git-srv moves database ownership between nodes (CLAUDE.md, “Deploying”) — a brief window where the first registry request to a moved image can fail once."
                : confirmOpen?.message}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setConfirmOpen(null)}>Cancel</Button>
            <Button
              type="button"
              onClick={() => {
                if (!confirmOpen) return;
                if (confirmOpen.needsSecond && !confirmOpen.onSecond) {
                  setConfirmOpen({ ...confirmOpen, onSecond: true });
                  return;
                }
                void doSave(confirmOpen.patch);
              }}
            >
              {confirmOpen?.needsSecond && !confirmOpen.onSecond ? "Continue" : "Save"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function RowInput({ row, value, onChange }: { row: SettingRow; value: unknown; onChange: (v: unknown) => void }) {
  if (row.unit === "bool") {
    return <Checkbox checked={Boolean(value)} onCheckedChange={(c) => onChange(c === true)} />;
  }
  if (row.unit === "string") {
    return (
      <Input
        className="h-8 w-56"
        value={typeof value === "string" ? value : ""}
        onChange={(e) => onChange(e.target.value)}
      />
    );
  }
  return (
    <Input
      type="number"
      className="h-8 w-32"
      min={row.range?.min}
      max={row.range?.max}
      value={typeof value === "number" ? value : Number(value ?? 0)}
      onChange={(e) => onChange(e.target.valueAsNumber)}
    />
  );
}
