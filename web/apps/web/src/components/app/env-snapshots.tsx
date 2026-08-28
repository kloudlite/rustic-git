"use client";

import { useActionState, useState } from "react";
import { Camera, Loader2, RotateCcw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { FastRefresh } from "@/components/app/fast-refresh";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { when } from "@/lib/time";
import {
  pushEnvironment, restoreEnvironmentFrom, type EnvActionState,
} from "@/app/(shell)/[owner]/(org)/environments/actions";

export type SnapshotNode = { id: string; message?: string; created_at: string };

/** One node's rail: the dot, and the line down to the next one. The last node draws no line —
 *  the lineage ends there, and a trailing stub reads as a snapshot that failed to load. */
function Rail({ last, pending = false }: { last: boolean; pending?: boolean }) {
  return (
    <span className="relative flex w-3 shrink-0 self-stretch justify-center" aria-hidden>
      <span
        className={`mt-1.5 size-2.5 shrink-0 rounded-full ${pending ? "bg-warning" : "bg-primary"}`}
      />
      {!last && <span className="absolute top-5 bottom-0 w-px bg-border" />}
    </span>
  );
}

/** Restore, in the one dialog both shapes share.
 *
 *  Live: the name is prefilled with the environment's own, because restoring IN PLACE is what
 *  people mean — and typing a different name is what makes it a new environment instead. The
 *  third button takes a snapshot of the current state first and waits for it to land, so
 *  "you can come back to it" is true rather than merely offered.
 *
 *  Archived: there is nothing to restore in place, so the name is empty and there is one button. */
function RestoreDialog({
  owner,
  id,
  snapshot,
  envName,
  current,
}: {
  owner: string;
  id: string;
  snapshot: SnapshotNode;
  /** `null` for an archived environment: no volume to restore into, so every restore is new. */
  envName: string | null;
  current: SnapshotNode | null;
}) {
  const [state, action, pending] = useActionState<EnvActionState, FormData>(restoreEnvironmentFrom, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  const label = snapshot.message || "snapshot";
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><RotateCcw />Restore</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Restore to &ldquo;{label}&rdquo;</DialogTitle>
            <DialogDescription>
              {envName && current ? (
                <>
                  Restoring discards every change made since &ldquo;{current.message || "snapshot"}&rdquo; (
                  {when(new Date(current.created_at).getTime())}). Take a snapshot of the current state
                  first, so you can come back to it?
                </>
              ) : (
                <>
                  A new environment, holding this exact snapshot&rsquo;s data, with the services the
                  push recorded — none, for a snapshot taken before they were.
                </>
              )}
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <input type="hidden" name="snapshotId" value={snapshot.id} />
          <input type="hidden" name="currentName" value={envName ?? ""} />
          <div className="grid gap-1.5">
            <Input
              name="name"
              defaultValue={envName ?? ""}
              placeholder="Name"
              autoFocus
              required
              className="h-9"
              aria-label="Restored environment name"
            />
            {envName && (
              <p className="text-caption text-muted-foreground">
                Same name restores in place. A different name restores into a new environment.
              </p>
            )}
          </div>
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            {envName ? (
              <>
                <Button type="submit" variant="outline" disabled={pending}>
                  {pending && <Loader2 className="animate-spin" />}Restore anyway
                </Button>
                {/* The safety snapshot rides as a form value on its own submit button, so ONE
                    action serves both answers and the two cannot drift apart. */}
                <Button type="submit" name="snapshotFirst" value="1" disabled={pending}>
                  {pending && <Loader2 className="animate-spin" />}Snapshot &amp; restore
                </Button>
              </>
            ) : (
              <Button type="submit" disabled={pending}>
                {pending && <Loader2 className="animate-spin" />}Restore
              </Button>
            )}
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function EnvSnapshots({
  owner,
  id,
  envName,
  pusher,
  history,
  currentId,
}: {
  owner: string;
  id: string;
  /** `null` for an archived environment — nothing to push, nothing to restore in place. */
  envName: string | null;
  pusher: string;
  history: SnapshotNode[];
  currentId: string | null;
}) {
  // Take a snapshot, from the top of the list. The api answers with the REQUEST's id — the record
  // only appears in the history once the push lands — so the request id, plus how long the history
  // was when it was made, is the whole "still uploading" test. Adjusted DURING render (React's
  // own pattern for state derived from a prop) rather than in an effect: an effect that sets state
  // renders twice and, on this one, would fight the 2 s poll it exists to drive.
  const [pushState, pushAction, pushing] = useActionState<EnvActionState, FormData>(pushEnvironment, null);
  const [asked, setAsked] = useState<{ request: string; had: number } | null>(null);
  if (pushState?.requestId && asked?.request !== pushState.requestId) {
    setAsked({ request: pushState.requestId, had: history.length });
  }
  const pendingNode = pushing || (asked !== null && history.length <= asked.had);

  const current = history.find((h) => h.id === currentId) ?? null;

  return (
    <>
      {/* Only while a push is in flight: the shell's 10 s poll would show a landed snapshot late,
          and this timer vanishes with the last pending node. */}
      {pendingNode && <FastRefresh />}
      {envName && (
        <form action={pushAction} className="mt-5 flex flex-wrap items-center gap-2">
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <Input
            name="message"
            placeholder="Message for the snapshot (optional)"
            aria-label="Message for the snapshot"
            className="h-8 max-w-sm text-sm2"
          />
          <Button type="submit" size="sm" disabled={pushing}>
            {pushing ? <Loader2 className="animate-spin" /> : <Camera />}Take snapshot
          </Button>
          {pushState?.error && (
            <p role="alert" className="w-full text-sm2 font-medium text-destructive">{pushState.error}</p>
          )}
        </form>
      )}

      {history.length === 0 && !pendingNode ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          No snapshots yet. Push the environment to take one.
        </p>
      ) : (
        <ul className="mt-5 divide-y divide-border border border-border bg-card">
          {pendingNode && (
            <li className="flex items-start gap-3.5 px-5 py-3.5">
              <Rail last={history.length === 0} pending />
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm2">Taking a snapshot</div>
                <div className="mt-0.5 text-caption text-muted-foreground">just now · {pusher}</div>
              </div>
              <span className="shrink-0 border border-border px-1.5 py-0.5 text-caption text-muted-foreground">
                uploading…
              </span>
            </li>
          )}
          {history.map((c, i) => {
            const at = new Date(c.created_at);
            // Archived: nothing is live, so nothing is current — every node is restorable.
            const isCurrent = envName !== null && (current ? c.id === current.id : i === 0);
            return (
              <li key={c.id} className="flex items-start gap-3.5 px-5 py-3.5">
                <Rail last={i === history.length - 1} />
                <div className="min-w-0 flex-1">
                  <div className={`truncate text-sm2 ${c.message ? "" : "text-muted-foreground italic"}`}>
                    {c.message || "snapshot"}
                  </div>
                  <div className="mt-0.5 text-caption text-muted-foreground">
                    <span title={at.toLocaleString("en")}>{when(at.getTime())}</span> ·{" "}
                    <span className="font-mono">{c.id.slice(0, 8)}</span> · {pusher}
                  </div>
                </div>
                {isCurrent ? (
                  <span className="shrink-0 border border-border px-1.5 py-0.5 text-caption text-muted-foreground">
                    current
                  </span>
                ) : (
                  <RestoreDialog owner={owner} id={id} snapshot={c} envName={envName} current={current ?? history[0]} />
                )}
              </li>
            );
          })}
        </ul>
      )}

      <p className="mt-3 text-caption text-muted-foreground">
        Newest first. <b>current</b> is the snapshot this environment last landed on; changes since
        it are not captured until you take a snapshot.
      </p>
    </>
  );
}
