"use client";

import { useActionState, useState } from "react";
import { Loader2, RotateCcw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import {
  restoreWorkspace, type WsActionState,
} from "@/app/(shell)/[owner]/(org)/workspaces/actions";

/** One row on the snapshots detail page (`snapshots/[id]/page.tsx`) — `id` there IS the
 *  source workspace's id (a volume's `name` is its owning workspace/environment's id).
 *  Builds a NEW workspace grafted onto this exact commit, not the source's current tip —
 *  see `crates/workspaces/src/api.rs::restore_ws`. Workspace snapshots only: environments
 *  have no `/restore` route. */
export function RestoreDialog({ owner, srcWorkspace, snapshotId }: { owner: string; srcWorkspace: string; snapshotId: string }) {
  const [open, setOpen] = useState(false);
  const [state, action, pending] = useActionState<WsActionState, FormData>(restoreWorkspace, null);
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
          <input type="hidden" name="srcWorkspace" value={srcWorkspace} />
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
