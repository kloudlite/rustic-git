"use client";

import { useActionState } from "react";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { Loader2, RotateCcw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import {
  restoreWorkspace, type WsActionState,
} from "@/app/(shell)/[owner]/(org)/workspaces/actions";

/** One row on the snapshots detail page (`snapshots/[id]/page.tsx`). Builds a NEW workspace
 *  grafted onto this exact commit, not the source's current tip — see
 *  `crates/workspaces/src/api.rs::restore_ws`. The snapshot id is the whole request: the api
 *  tier finds the volume it belongs to, so this works when the source workspace is gone,
 *  which is when a restore is most wanted. The new workspace gets the standard quota in that
 *  case; no field, because a person restoring a lost workspace is not sizing a disk. */
export function RestoreDialog({ owner, snapshotId }: { owner: string; snapshotId: string }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(restoreWorkspace, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><RotateCcw />Restore</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Restore snapshot</DialogTitle>
            <DialogDescription>A new workspace, grafted onto this exact snapshot.</DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="snapshotId" value={snapshotId} />
          <Input name="name" placeholder="Name" autoFocus className="h-9" />
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Restore</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
