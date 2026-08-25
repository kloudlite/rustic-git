"use client";

import { useActionState, useMemo, useState } from "react";
import { GitBranch, Loader2, Play, Plus, Search, Square, SquareTerminal, Upload } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import type { ApiWorkspace } from "@/lib/api";
import {
  cloneWorkspace, commitWorkspace, pushWorkspace, startWorkspace, stopWorkspace, type WsActionState,
} from "@/app/(shell)/[owner]/(org)/workspaces/actions";

/** Push, clone, start and stop all take one hidden pair of ids and nothing
 *  else, so one inline form (no dialog) does each — same idiom as
 *  `pull-actions.tsx`'s bare `useActionState` forms. Commit and clone need a
 *  value first, so those two get a small dialog apiece instead. */
function PushForm({ owner, id }: { owner: string; id: string }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(pushWorkspace, null);
  return (
    <form action={action} className="contents">
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="id" value={id} />
      <Button type="submit" variant="outline" size="sm" disabled={pending} title={state?.error}>
        {pending ? <Loader2 className="animate-spin" /> : <Upload />}Push
      </Button>
    </form>
  );
}

function StartForm({ owner, id }: { owner: string; id: string }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(startWorkspace, null);
  return (
    <form action={action} className="contents">
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="id" value={id} />
      <Button type="submit" variant="outline" size="sm" disabled={pending} title={state?.error}>
        {pending ? <Loader2 className="animate-spin" /> : <Play />}Start
      </Button>
    </form>
  );
}

function StopForm({ owner, id }: { owner: string; id: string }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(stopWorkspace, null);
  return (
    <form action={action} className="contents">
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="id" value={id} />
      <Button type="submit" variant="outline" size="sm" disabled={pending} title={state?.error}>
        {pending ? <Loader2 className="animate-spin" /> : <Square />}Stop
      </Button>
    </form>
  );
}

function CommitDialog({ owner, id }: { owner: string; id: string }) {
  const [open, setOpen] = useState(false);
  const [state, action, pending] = useActionState<WsActionState, FormData>(commitWorkspace, null);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><GitBranch />Commit</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Commit</DialogTitle>
            <DialogDescription>A local snapshot, not yet pushed to the volume registry.</DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <Textarea name="message" placeholder="Message (optional)" rows={3} />
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Commit</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function CloneDialog({ owner, id }: { owner: string; id: string }) {
  const [open, setOpen] = useState(false);
  const [state, action, pending] = useActionState<WsActionState, FormData>(cloneWorkspace, null);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><Plus />Clone</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Clone workspace</DialogTitle>
            <DialogDescription>A new workspace, starting from this one&rsquo;s current volume.</DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <Input name="name" placeholder="Name" autoFocus className="h-9" />
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

/** Same filter idiom as `repo-list.tsx`: the whole list is already here, so
 *  filtering it locally is both simpler and faster than a round trip. */
export function WorkspaceList({ owner, workspaces }: { owner: string; workspaces: ApiWorkspace[] }) {
  const [q, setQ] = useState("");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return workspaces;
    return workspaces.filter((w) => w.name.toLowerCase().includes(needle));
  }, [workspaces, q]);

  if (workspaces.length === 0) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <SquareTerminal className="mx-auto size-6 text-muted-foreground" aria-hidden />
        <p className="mt-3 text-sm2 font-medium">No workspaces yet</p>
        <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
          A workspace is a machine, provisioned for one region and one quota.
        </p>
      </div>
    );
  }

  return (
    <>
      <div className="relative w-full max-w-xs">
        <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
        <Input
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="Filter workspaces"
          aria-label="Filter workspaces"
          className="h-8 pl-8 text-sm2"
        />
      </div>

      {shown.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          Nothing matches that.
        </p>
      ) : (
        <ul className="mt-5 divide-y divide-border border border-border bg-card">
          {shown.map((w) => (
            <li key={w.id} className="flex flex-wrap items-center gap-4 px-5 py-4">
              <span className="min-w-0 flex-1">
                <span className="flex items-center gap-2.5">
                  <span className="truncate text-body font-medium">{w.name}</span>
                  <WsEnvStateBadge state={w.state} />
                </span>
                <span className="mt-1 block text-sm2 text-muted-foreground">
                  {w.region} · {w.quota_gb} GB · {w.image}
                </span>
              </span>
              <div className="flex shrink-0 items-center gap-2">
                {w.state === "stopped" ? <StartForm owner={owner} id={w.id} /> : <StopForm owner={owner} id={w.id} />}
                <CommitDialog owner={owner} id={w.id} />
                <PushForm owner={owner} id={w.id} />
                <CloneDialog owner={owner} id={w.id} />
              </div>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
