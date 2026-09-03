"use client";

import { useActionState } from "react";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { Loader2, RotateCcw, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import {
  deleteWorkspaceSnapshot, restoreWorkspace, type WsActionState,
} from "@/app/(shell)/[owner]/(org)/workspaces/actions";
import type { SnapshotState } from "@/lib/snapshot-state";
import { deleteVolumeCopy } from "@/lib/archived";

/** A row on a workspace's own snapshots page (`workspaces/[id]/snapshots`). Builds a NEW
 *  workspace grafted onto this exact snapshot, not the source's current tip — see
 *  `crates/workspaces/src/api.rs::restore_ws`. Restoring in place is deliberately not offered.
 *
 *  Only reachable from the owner's own workspace row: a workspace's snapshots are that person's
 *  undo history, and the api scopes the lookup to volumes under their own owner label, so a
 *  teammate asking for the id gets the same 404 a stranger does. */
export function RestoreDialog({
  owner,
  snapshotId,
  state: snapshot,
}: {
  owner: string;
  snapshotId: string;
  /** The snapshot's frozen definition, when it has one. Absent on older snapshots, and the
   *  dialog then asks for nothing but a name: the api restores the snapshot's own definition. */
  state?: SnapshotState | null;
}) {
  // Only a workspace state pre-fills — an environment's definition is its services, which this
  // dialog does not build.
  const def = snapshot?.kind === "workspace" ? snapshot : null;
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
          <Input name="name" placeholder="Name" aria-label="Name" autoFocus required className="h-9" />
          {def && (
            <div className="grid gap-2">
              <Input
                name="image"
                aria-label="Image"
                defaultValue={def.image}
                placeholder="Image"
                className="h-9"
              />
              <Input
                name="packages"
                aria-label="Packages"
                defaultValue={def.packages.join(", ")}
                placeholder="Packages, comma separated"
                className="h-9"
              />
              {/* Read-only: the restored volume inherits the snapshot's quota, and offering to
                  change it here would be a second, silent resize. */}
              <p className="text-sm2 text-muted-foreground">Disk {def.quotaGb} GB, as the snapshot had it.</p>
            </div>
          )}
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

/** Delete ONE snapshot. A snapshot is kept until it is explicitly deleted, and this is that
 *  delete: the bytes go with it, which is why the copy says so plainly rather than talking about
 *  a record. The workspace's own live disk, if it still has one, is untouched. */
export function DeleteSnapshotDialog({
  owner,
  id,
  snapshotId,
  label,
}: {
  owner: string;
  id: string;
  snapshotId: string;
  /** The message, or the short id when there is none — the dialog has to name ONE of several
   *  snapshots that may all be message-less. */
  label: string;
}) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(deleteWorkspaceSnapshot, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="ghost" size="sm" className="text-destructive" aria-label={`Delete snapshot ${label}`}>
          <Trash2 />
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Delete snapshot &ldquo;{label}&rdquo;?</DialogTitle>
            <DialogDescription>
              {deleteVolumeCopy(1)} The workspace itself is not affected.
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <input type="hidden" name="snapshotId" value={snapshotId} />
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" variant="destructive" disabled={pending}>
              {pending && <Loader2 className="animate-spin" />}Delete
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
