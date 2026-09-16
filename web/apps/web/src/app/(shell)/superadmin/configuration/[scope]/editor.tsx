"use client";

import { useState, useTransition } from "react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import type { SettingsSchemaRow } from "@/lib/api";
import { canSave, changedFields, fmt, initialDraft, type Draft } from "@/lib/settings";
import { revertSettingsAction, saveSettingsAction } from "../../actions";
import { Section } from "../../ui/section";
import { DataTable, EmptyState, RowActions, Td, Th, Tr } from "../../ui/data-table";
import { Pill } from "../../ui/pill";

/** Every field of one scope as a control, one note, one Save that sends only what moved. */
export function SettingsEditor({
  scope,
  rows,
  stored,
  history,
}: {
  scope: string;
  rows: SettingsSchemaRow[];
  stored: Record<string, unknown>;
  history: Record<string, unknown>[];
}) {
  const [draft, setDraft] = useState<Draft>({});
  const [note, setNote] = useState("");
  const [status, setStatus] = useState<{ ok: boolean; text: string } | null>(null);
  const [revertAt, setRevertAt] = useState<number | null>(null);
  const [revertNote, setRevertNote] = useState("");
  const [revertError, setRevertError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  const changes = changedFields(rows, stored, draft);
  const set = (name: string, v: string | boolean) => {
    setDraft((d) => ({ ...d, [name]: v }));
    setStatus(null);
  };

  function save() {
    if (!canSave(changes, note)) return;
    startTransition(async () => {
      const r = await saveSettingsAction(scope, changes, note);
      if (!r.ok) return setStatus({ ok: false, text: r.message });
      setDraft({});
      setNote("");
      setStatus({ ok: true, text: "Saved" });
    });
  }

  function revert() {
    if (revertAt === null || revertNote.trim() === "") return;
    startTransition(async () => {
      const r = await revertSettingsAction(scope, revertAt, revertNote.trim());
      if (!r.ok) return setRevertError(r.message);
      setRevertAt(null);
      setDraft({});
      setStatus({ ok: true, text: "Reverted" });
    });
  }

  return (
    <div className="space-y-4">
      <Section eyebrow="Settings" title={scope === "central" ? "Central" : scope} count={`${rows.length} fields`} bare>
        <DataTable>
          <thead>
            <tr>
              <Th>Setting</Th>
              <Th>Takes effect</Th>
              <Th numeric>Default</Th>
              <Th>Value</Th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => {
              const value = draft[row.name] ?? initialDraft(row, stored[row.name]);
              const isStored = stored[row.name] !== null && stored[row.name] !== undefined;
              return (
                <Tr key={row.name}>
                  <Td>
                    <div className="font-medium">{row.name}</div>
                    <div className="max-w-md text-caption text-muted-foreground">{row.description}</div>
                  </Td>
                  <Td>
                    {row.mark === "boot" ? (
                      <Pill tone="warn">restarts {row.readers.join(", ") || "its readers"} on save</Pill>
                    ) : (
                      <Pill tone="neutral">live</Pill>
                    )}
                  </Td>
                  <Td numeric className="text-muted-foreground">
                    {fmt(row.env ?? row.default)}
                    {row.range && <div className="text-caption">{row.range.min} – {row.range.max}</div>}
                  </Td>
                  <Td>
                    {typeof value === "boolean" ? (
                      <label className="flex items-center gap-2 text-sm2">
                        <Checkbox checked={value} onCheckedChange={(c) => set(row.name, c === true)} aria-label={row.name} />
                        {value ? "On" : "Off"}
                      </label>
                    ) : (
                      <Input
                        value={value}
                        onChange={(e) => set(row.name, e.target.value)}
                        type={row.unit === "string" ? "text" : "number"}
                        min={row.range?.min}
                        max={row.range?.max}
                        placeholder={isStored ? "" : `default: ${fmt(row.env ?? row.default)}`}
                        aria-label={row.name}
                        className="h-8 w-64"
                      />
                    )}
                    {!isStored && <div className="text-caption text-muted-foreground">not set, using the default</div>}
                  </Td>
                </Tr>
              );
            })}
          </tbody>
        </DataTable>
      </Section>

      <div className="space-y-2">
        <Textarea
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder="Why this change? Required, and recorded with the save."
          rows={2}
          aria-label="Note"
        />
        <div className="flex items-center gap-3">
          <Button type="button" onClick={save} disabled={pending || !canSave(changes, note)}>
            {Object.keys(changes).length
              ? `Save ${Object.keys(changes).length} change${Object.keys(changes).length === 1 ? "" : "s"}`
              : "Save"}
          </Button>
          {status && (
            <p role={status.ok ? "status" : "alert"} className={status.ok ? "text-sm2 text-muted-foreground" : "text-sm2 font-medium text-destructive"}>
              {status.text}
            </p>
          )}
        </div>
      </div>

      <Section eyebrow="History" title="Earlier versions" count={history.length} bare>
        {history.length === 0 ? (
          <EmptyState>No earlier version yet. Each save keeps the one before it, up to ten.</EmptyState>
        ) : (
          <DataTable>
            <thead>
              <tr>
                <Th>Version</Th>
                <Th>Values set</Th>
                <Th />
              </tr>
            </thead>
            <tbody>
              {history.map((h, i) => (
                <Tr key={i}>
                  <Td>{i === 0 ? "Before the last save" : `${i + 1} saves ago`}</Td>
                  <Td className="font-mono text-caption text-muted-foreground">
                    {Object.entries(h).filter(([k, v]) => v !== null && k !== "updatedBy" && k !== "updatedAt").map(([k, v]) => `${k}=${fmt(v)}`).join("  ") || "all defaults"}
                  </Td>
                  <Td>
                    {/* Central's revert names no index: it only ever undoes the last save. */}
                    {(scope !== "central" || i === 0) && (
                      <RowActions>
                        <button
                          type="button"
                          className="text-sm2 text-muted-foreground hover:text-foreground"
                          onClick={() => { setRevertAt(i); setRevertNote(""); setRevertError(null); }}
                        >
                          Revert
                        </button>
                      </RowActions>
                    )}
                  </Td>
                </Tr>
              ))}
            </tbody>
          </DataTable>
        )}
      </Section>

      <Dialog open={revertAt !== null} onOpenChange={(open) => !open && setRevertAt(null)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>Revert to this version?</DialogTitle>
            <DialogDescription>
              Every setting goes back to what this version held; a setting it left unset returns to its default. The current values are kept in history.
            </DialogDescription>
          </DialogHeader>
          <Textarea value={revertNote} onChange={(e) => setRevertNote(e.target.value)} placeholder="Why revert?" rows={3} aria-label="Why revert?" />
          {revertError && <p role="alert" className="text-sm2 font-medium text-destructive">{revertError}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setRevertAt(null)}>Cancel</Button>
            <Button type="button" onClick={revert} disabled={pending || revertNote.trim() === ""}>Revert</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
