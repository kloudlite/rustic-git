"use client";

import { useState, useTransition } from "react";
import { useRouter } from "next/navigation";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import { activateRegionAction, deactivateRegionAction } from "../actions";

/** Activate is one write, no reason (restoring what was already registered). Deactivate is the
 *  loud half — a required note plus a second confirmation naming the consequence, per the Global
 *  Constraint ("deactivating a region"). */
export function RegionStatusToggle({ region, status }: { region: string; status: string }) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [confirmedFirst, setConfirmedFirst] = useState(false);
  const [note, setNote] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [pending, startTransition] = useTransition();

  if (status !== "active") {
    return (
      <Button
        type="button"
        size="sm"
        variant="outline"
        disabled={pending}
        onClick={() =>
          startTransition(async () => {
            const r = await activateRegionAction(region);
            if (!r.ok) setError(r.message);
            else router.refresh();
          })
        }
      >
        {pending && <Loader2 className="animate-spin" />}
        Activate
      </Button>
    );
  }

  function submit() {
    if (note.trim() === "") return;
    if (!confirmedFirst) {
      setConfirmedFirst(true);
      return;
    }
    startTransition(async () => {
      const r = await deactivateRegionAction(region, note.trim());
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
      {error && <span className="mr-2 text-caption text-destructive">{error}</span>}
      <Button
        type="button"
        size="sm"
        variant="outline"
        onClick={() => {
          setOpen(true);
          setConfirmedFirst(false);
          setNote("");
          setError(null);
        }}
      >
        Deactivate
      </Button>
      <Dialog open={open} onOpenChange={(o) => !o && setOpen(false)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{confirmedFirst ? `This stops ${region} being offered` : `Deactivate ${region}`}</DialogTitle>
            <DialogDescription>
              {confirmedFirst
                ? "New workspaces and environments can no longer be placed in this region. Anything already running there is unaffected."
                : "A reason is required — it's recorded on the region alongside who and when."}
            </DialogDescription>
          </DialogHeader>
          {!confirmedFirst && (
            <Textarea value={note} onChange={(e) => setNote(e.target.value)} placeholder="Why is this region being deactivated?" rows={3} />
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="button" onClick={submit} disabled={pending || note.trim() === ""}>
              {pending && <Loader2 className="animate-spin" />}
              {confirmedFirst ? "Continue" : "Deactivate"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
