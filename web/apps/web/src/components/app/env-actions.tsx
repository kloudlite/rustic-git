"use client";

import { useActionState } from "react";
import { usePathname } from "next/navigation";
import { Camera, Loader2, Play, Plus, Square, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import {
  cloneEnvironment, deleteEnvironment, deleteEnvironmentSnapshots, pushEnvironment, startEnvironment,
  stopEnvironment, type EnvActionState,
} from "@/app/(shell)/[owner]/(org)/environments/actions";
import type { EnvState } from "@/lib/api";
import { deleteVolumeCopy, keptSnapshotsCopy } from "@/lib/archived";

/** Start/stop, the bare-form idiom: one hidden id, no dialog, since neither takes a value. */
function ToggleForm({ owner, id, running }: { owner: string; id: string; running: boolean }) {
  const action = running ? stopEnvironment : startEnvironment;
  const [state, act, pending] = useActionState<EnvActionState, FormData>(action, null);
  return (
    <form action={act}>
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="id" value={id} />
      <Button type="submit" variant="outline" size="sm" disabled={pending}>
        {pending ? <Loader2 className="animate-spin" /> : running ? <Square /> : <Play />}
        {running ? "Stop" : "Start"}
      </Button>
      {state?.error && <p role="alert" className="mt-1 text-caption font-medium text-destructive">{state.error}</p>}
      {state?.warning && <p role="status" className="mt-1 text-caption font-medium text-warning">{state.warning}</p>}
    </form>
  );
}

/** A push with an optional message. The api answers with the REQUEST's id, never the snapshot —
 *  the record shows up in the Snapshots tab when the push lands, which is what that tab polls
 *  for. So this dialog's job ends at "asked". */
function PushDialog({ owner, id }: { owner: string; id: string }) {
  const [state, action, pending] = useActionState<EnvActionState, FormData>(pushEnvironment, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><Camera />Push snapshot</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Push a snapshot</DialogTitle>
            <DialogDescription>
              One snapshot of the environment&rsquo;s whole volume, every service&rsquo;s data
              included, taken at the same instant.
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <Input name="message" placeholder="Message (optional)" aria-label="Message" autoFocus className="h-9" />
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Push</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** The deleted environment's one action: its snapshots are the last copy of that data, and the
 *  only thing keeping its volume. */
export function DeleteSnapshotsDialog({
  owner,
  id,
  name,
  snapshots,
}: {
  owner: string;
  id: string;
  name: string;
  snapshots: number;
}) {
  const [state, action, pending] = useActionState<EnvActionState, FormData>(deleteEnvironmentSnapshots, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className="text-destructive"><Trash2 />Delete snapshots</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Delete {name}&rsquo;s snapshots?</DialogTitle>
            <DialogDescription>
              {deleteVolumeCopy(snapshots)} This is the last copy of that environment&rsquo;s data.
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
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

/** A name prompt and nothing else — a new environment from this one's current volume. */
function CloneEnvDialog({ owner, id }: { owner: string; id: string }) {
  const [state, action, pending] = useActionState<EnvActionState, FormData>(cloneEnvironment, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><Plus />Clone</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Clone environment</DialogTitle>
            <DialogDescription>A new environment, starting from this one&rsquo;s current volume.</DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <Input name="name" placeholder="Name" aria-label="Name" autoFocus required className="h-9" />
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Clone</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** Delete always keeps its snapshots — the environment then appears under Snapshots, and
 *  deleting them for good lives there. So this dialog states what survives rather than offering
 *  a second, irreversible choice inside a reversible one. */
export function DeleteEnvDialog({
  owner,
  id,
  name,
  snapshots,
}: {
  owner: string;
  id: string;
  name: string;
  /** How many snapshots the environment has, when the caller knows — the count-free sentence
   *  otherwise, which is vaguer but never wrong. */
  snapshots?: number | null;
}) {
  const [state, action, pending] = useActionState<EnvActionState, FormData>(deleteEnvironment, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className="text-destructive"><Trash2 />Delete</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Delete {name}?</DialogTitle>
            <DialogDescription>
              Stops its services, pushes one final snapshot of its volume, then removes it from
              the node.
            </DialogDescription>
          </DialogHeader>
          <p className="text-sm2 text-muted-foreground">{keptSnapshotsCopy(snapshots)}</p>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
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

/** `state === null` is an ARCHIVED environment: there is nothing to start, stop or push, because
 *  there is no environment — only its snapshots. */
export function EnvHeaderActions({
  owner,
  id,
  state,
}: {
  owner: string;
  id: string;
  state: EnvState | null;
}) {
  // Deleting — the environment or its snapshots — lives on the Settings tab, where every other
  // destructive action in the product lives. The header is what you DO with a running thing.
  // The Snapshots tab has its own push form, and that one owns the pending row: a push from
  // here would land with no "uploading…" node and no fast poll. So there it is the only one.
  const onSnapshots = usePathname().endsWith("/snapshots");
  if (state === null) return null;
  return (
    <>
      <ToggleForm owner={owner} id={id} running={state === "running"} />
      {!onSnapshots && <PushDialog owner={owner} id={id} />}
      <CloneEnvDialog owner={owner} id={id} />
    </>
  );
}
