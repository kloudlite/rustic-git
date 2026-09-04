"use client";

import { useState, useTransition } from "react";
import { useRouter } from "next/navigation";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Badge } from "@/components/ui/badge";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import type { SuperAdmin } from "@/lib/api";
import { removeDisabledReason } from "@/lib/access";
import { addSuperadminAction, removeSuperadminAction } from "../actions";

// The bootstrap path (`Directory::bootstrap_superadmins`, `crates/pulls/src/directory/mod.rs`)
// writes this literal as `addedBy` — nothing else does, so it doubles as the badge condition.
const BOOTSTRAP = "bootstrap";

function AddForm() {
  const router = useRouter();
  const [email, setEmail] = useState("");
  const [note, setNote] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  function submit() {
    if (email.trim() === "" || note.trim() === "") return;
    startTransition(async () => {
      const r = await addSuperadminAction(email.trim(), note.trim());
      if (!r.ok) {
        setError(r.message);
        return;
      }
      setEmail("");
      setNote("");
      setError(null);
      router.refresh();
    });
  }

  return (
    <div className="flex flex-col gap-2 border border-border bg-card p-4">
      <div className="flex flex-col gap-2 sm:flex-row">
        <Input
          value={email}
          onChange={(e) => setEmail(e.target.value)}
          placeholder="person@example.com"
          type="email"
          className="sm:max-w-xs"
        />
        <Textarea
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder="Why is this person becoming a superadmin?"
          rows={1}
          className="sm:flex-1"
        />
        <Button type="button" onClick={submit} disabled={pending || email.trim() === "" || note.trim() === ""}>
          {pending && <Loader2 className="animate-spin" />}
          Add
        </Button>
      </div>
      {error && <p className="text-caption text-destructive">{error}</p>}
    </div>
  );
}

function RemoveButton({ row, disabledReason }: { row: SuperAdmin; disabledReason: string | null }) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [confirmedFirst, setConfirmedFirst] = useState(false);
  const [note, setNote] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  function openDialog() {
    setOpen(true);
    setConfirmedFirst(false);
    setNote("");
    setError(null);
  }

  function submit() {
    if (note.trim() === "") return;
    if (!confirmedFirst) {
      setConfirmedFirst(true);
      return;
    }
    startTransition(async () => {
      const r = await removeSuperadminAction(row._id, note.trim());
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
        disabled={disabledReason !== null}
        title={disabledReason ?? undefined}
        onClick={openDialog}
      >
        Remove
      </Button>
      {open && (
        <Dialog open onOpenChange={(o) => !o && setOpen(false)}>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>{confirmedFirst ? `Remove ${row._id}` : `Remove ${row._id}?`}</DialogTitle>
              <DialogDescription>
                {confirmedFirst
                  ? "They lose access to every superadmin page and route immediately."
                  : "A reason is required — it's recorded on the audit row alongside who and when."}
              </DialogDescription>
            </DialogHeader>
            {!confirmedFirst && (
              <Textarea value={note} onChange={(e) => setNote(e.target.value)} placeholder="Why is this person being removed?" rows={3} />
            )}
            {error && <p className="text-caption text-destructive">{error}</p>}
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
              <Button type="button" variant="destructive" onClick={submit} disabled={pending || note.trim() === ""}>
                {pending && <Loader2 className="animate-spin" />}
                {confirmedFirst ? "Continue" : "Remove"}
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}
    </>
  );
}

export function AccessTable({ rows, selfEmail }: { rows: SuperAdmin[]; selfEmail: string }) {
  return (
    <div className="flex flex-col gap-4">
      <AddForm />
      <div className="overflow-x-auto border border-border bg-card">
        <table className="w-full text-sm2">
          <thead>
            <tr className="border-b border-border text-left text-caption text-muted-foreground">
              <th className="px-3 py-2 font-medium">Email</th>
              <th className="px-3 py-2 font-medium">Added by</th>
              <th className="px-3 py-2 font-medium">Added at</th>
              <th className="px-3 py-2 font-medium" />
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {rows.map((row) => {
              const disabledReason = removeDisabledReason(row, rows, selfEmail);
              return (
                <tr key={row._id}>
                  <td className="px-3 py-2">{row._id}</td>
                  <td className="px-3 py-2">
                    {row.addedBy === BOOTSTRAP ? (
                      <span className="flex items-center gap-1.5">
                        {row.addedBy}
                        <Badge variant="outline">bootstrap</Badge>
                      </span>
                    ) : (
                      row.addedBy
                    )}
                  </td>
                  <td className="px-3 py-2 text-muted-foreground">{new Date(row.addedAt).toLocaleString()}</td>
                  <td className="px-3 py-2 text-right">
                    <RemoveButton row={row} disabledReason={disabledReason} />
                  </td>
                </tr>
              );
            })}
            {rows.length === 0 && (
              <tr>
                <td colSpan={4} className="px-3 py-8 text-center text-muted-foreground">No superadmins reported.</td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
