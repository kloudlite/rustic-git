"use client";

import { useActionState } from "react";
import { Camera, Loader2, Play, Square, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import {
  deleteEnvironmentSnapshots, pushEnvironment, startEnvironment, stopEnvironment, type EnvActionState,
} from "@/app/(shell)/[owner]/(org)/environments/actions";
import type { EnvState } from "@/lib/api";

/** Start/stop, the bare-form idiom: one hidden id, no dialog, since neither takes a value. */
function ToggleForm({ owner, id, running }: { owner: string; id: string; running: boolean }) {
  const action = running ? stopEnvironment : startEnvironment;
  const [state, act, pending] = useActionState<EnvActionState, FormData>(action, null);
  return (
    <form action={act}>
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="id" value={id} />
      <Button type="submit" variant="outline" size="sm" disabled={pending} title={state?.error}>
        {pending ? <Loader2 className="animate-spin" /> : running ? <Square /> : <Play />}
        {running ? "Stop" : "Start"}
      </Button>
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
          <Input name="message" placeholder="Message (optional)" autoFocus className="h-9" />
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

/** The archived environment's one action: its snapshots are the last copy of that data. */
function DeleteSnapshotsDialog({ owner, id, name }: { owner: string; id: string; name: string }) {
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
              Permanent. This is the last copy of that environment&rsquo;s data — nothing else
              references it once the row is gone.
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

/** `state === null` is an ARCHIVED environment: there is nothing to start, stop or push, because
 *  there is no environment — only its snapshots. */
export function EnvHeaderActions({
  owner,
  id,
  name,
  state,
}: {
  owner: string;
  id: string;
  name: string;
  state: EnvState | null;
}) {
  if (state === null) return <DeleteSnapshotsDialog owner={owner} id={id} name={name} />;
  return (
    <>
      <ToggleForm owner={owner} id={id} running={state === "running"} />
      <PushDialog owner={owner} id={id} />
    </>
  );
}
