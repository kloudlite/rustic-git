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
import {
  restoreEnvironment, type EnvActionState,
} from "@/app/(shell)/[owner]/(org)/environments/actions";

/** A row on a workspace's own snapshots page (`workspaces/[id]/snapshots`). Builds a NEW
 *  workspace grafted onto this exact commit, not the source's current tip — see
 *  `crates/workspaces/src/api.rs::restore_ws`. Restoring in place is deliberately not offered.
 *
 *  Only reachable from the owner's own workspace row: a workspace's snapshots are that person's
 *  undo history, and the api scopes the lookup to volumes under their own owner label, so a
 *  teammate asking for the id gets the same 404 a stranger does. */
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

/** The same, for a row on an environment's snapshots page. The snapshot id is the whole request:
 *  the api finds the volume it belongs to, so this works when the source environment is gone —
 *  an ARCHIVED row — which is when a restore is most wanted.
 *
 *  No services field. A push writes the environment's services into the record's provenance, so a
 *  restore reproduces them where they were recorded; a record written before that carries none and
 *  the new environment starts with no services. The dialog says both out loud rather than
 *  promising one of them. */
export function RestoreEnvDialog({ owner, snapshotId }: { owner: string; snapshotId: string }) {
  const [state, action, pending] = useActionState<EnvActionState, FormData>(restoreEnvironment, null);
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
            <DialogDescription>
              A new environment, holding this exact snapshot&rsquo;s data, with the services the
              push recorded — none, for a snapshot taken before they were.
            </DialogDescription>
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
