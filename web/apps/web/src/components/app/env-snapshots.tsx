"use client";

import { useActionState, useEffect, useState } from "react";
import { Camera, Loader2, RotateCcw, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { FastRefresh } from "@/components/app/fast-refresh";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { when } from "@/lib/time";
import {
  deleteEnvironmentSnapshot, pushEnvironment, restoreEnvironmentFrom, type EnvActionState,
} from "@/app/(shell)/[owner]/(org)/environments/actions";

export type SnapshotNode = { id: string; message?: string; created_at: string; parent: string | null };

/** Every node is this card. Children hang below it in a nested list (`Branch`), so the tree
 *  needs no rail geometry: the list's own left border IS the line to the children. */
function Node({ current, children }: { current?: boolean; children: React.ReactNode }) {
  return (
    <li className={`relative border border-border px-4 py-3 ${current ? "bg-primary/5" : "bg-card"}`}>
      {current && <span aria-hidden className="absolute inset-y-0 left-0 w-0.5 bg-primary" />}
      {children}
    </li>
  );
}

/** A node's children: indented under it, joined to it by the list's left border. */
function Branch({ children }: { children: React.ReactNode }) {
  return <ul className="ml-5 grid gap-3 border-l-2 border-border pl-5">{children}</ul>;
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

/** Delete ONE record. Deliberately worded as removing a RECORD: nothing about the environment's
 *  disk changes, and the current node says so a second time — the lineage stops showing where the
 *  environment sits, which is the only thing that actually goes. */
function DeleteSnapshotDialog({
  owner,
  id,
  snapshot,
  isCurrent,
}: {
  owner: string;
  id: string;
  snapshot: SnapshotNode;
  isCurrent: boolean;
}) {
  const [state, action, pending] = useActionState<EnvActionState, FormData>(deleteEnvironmentSnapshot, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  // The id, not the word "snapshot": the dialog names ONE record among several that may all be
  // message-less, and "Delete snapshot “snapshot”?" names none of them.
  const label = snapshot.message || snapshot.id.slice(0, 8);
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
            <DialogTitle>Delete snapshot &ldquo;{label}&rdquo;</DialogTitle>
            <DialogDescription>
              Delete snapshot &ldquo;{label}&rdquo; ({when(new Date(snapshot.created_at).getTime())})? The
              record is removed from the lineage; the environment&rsquo;s disk is not affected.
              {isCurrent && (
                <>
                  {" "}
                  This is the snapshot the environment currently sits on; deleting the record does not
                  change the disk, but the lineage will no longer show where it is.
                </>
              )}
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <input type="hidden" name="snapshotId" value={snapshot.id} />
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

export function EnvSnapshots({
  owner,
  id,
  envName,
  pusher,
  history,
  restoredTo,
  restoredAt,
}: {
  owner: string;
  id: string;
  /** `null` for an archived environment — nothing to push, nothing to restore in place. */
  envName: string | null;
  pusher: string;
  history: SnapshotNode[];
  /** The Volume's `restoredTo`/`restoreRequestedAt`: where an in-place restore put the disk. */
  restoredTo: string | null;
  restoredAt: string | null;
}) {
  // Take a snapshot, from the live node. The api answers with the REQUEST's id — the record only
  // appears in the history once the push lands — so the request id, plus how long the history was
  // when it was made, is the whole "still uploading" test. Adjusted DURING render (React's own
  // pattern for state derived from a prop) rather than in an effect: an effect that sets state
  // renders twice and, on this one, would fight the 2 s poll it exists to drive.
  const [pushState, pushAction, pushing] = useActionState<EnvActionState, FormData>(pushEnvironment, null);
  const [asked, setAsked] = useState<{ request: string; had: number } | null>(null);
  const [stale, setStale] = useState(false);
  if (pushState?.requestId && asked?.request !== pushState.requestId) {
    setAsked({ request: pushState.requestId, had: history.length });
    setStale(false);
  }
  const waiting = asked !== null && history.length <= asked.had;
  // A push that FAILS leaves its SnapshotRequest in `error` and writes no record at all, so
  // "uploading…" would spin forever on a page nobody ever told. Five minutes, then say so and stop
  // polling. ponytail: a wall-clock deadline rather than the request's own status — follow that
  // instead once `/v1` projects a SnapshotRequest by id.
  useEffect(() => {
    if (!waiting) return;
    const t = setTimeout(() => setStale(true), 5 * 60_000);
    return () => clearTimeout(t);
  }, [waiting, asked?.request]);
  const pendingNode = pushing || (waiting && !stale);

  const byId = new Map(history.map((h) => [h.id, h]));
  const descends = (n: SnapshotNode, anc: string): boolean => {
    for (let p: SnapshotNode | undefined = n; p; p = p.parent ? byId.get(p.parent) : undefined) {
      if (p.id === anc) return true;
    }
    return false;
  };
  const restored = restoredTo ? (byId.get(restoredTo) ?? null) : null;
  // Where the environment sits. Never restored: the newest record (one straight chain). Restored:
  // the newest record pushed AFTER the restore that descends from the restored one — the
  // environment moved on to it — else the restored record itself. Its older children are the
  // branches the environment left behind.
  const since = restoredAt ? Date.parse(restoredAt) : 0;
  const current: SnapshotNode | null =
    envName === null
      ? null
      : restoredTo === null
        ? (history[0] ?? null)
        : restored === null
          ? null
          : (history.find((h) => Date.parse(h.created_at) > since && descends(h, restored.id)) ?? restored);
  // A `restoredTo` that names no record here: a restore grafted ANOTHER volume's snapshot in
  // place. Saying so is the honest answer — badging any record `current` would claim the
  // environment is on a snapshot it is not.
  const foreignCurrent = restoredTo !== null && restored === null ? restoredTo : null;

  // Oldest first, and the branch the environment is on LAST among siblings, so the live node is
  // the bottom of the tree rather than buried between two branches.
  const childrenOf = (parent: string | null) =>
    history
      .filter((h) => (h.parent && byId.has(h.parent) ? h.parent : null) === parent)
      .sort((a, b) => {
        const onPath = (n: SnapshotNode) => (current && descends(current, n.id) ? 1 : 0);
        return onPath(a) - onPath(b) || Date.parse(a.created_at) - Date.parse(b.created_at);
      });

  const live = envName && (
    <Node>
      <div className="min-w-0 flex-1">
        <div className="text-sm2 font-medium">Live environment</div>
        <div className="mt-0.5 text-caption text-muted-foreground">
          {history.length === 0 ? (
            "No snapshots yet — take one to start the lineage"
          ) : current ? (
            <>
              changes since{" "}
              <span>&ldquo;{current.message || "snapshot"}&rdquo;</span>{" "}
              (<span title={new Date(current.created_at).toLocaleString("en")}>
                {when(new Date(current.created_at).getTime())}
              </span>) are not snapshotted
            </>
          ) : (
            // Neutral on purpose: `restored_to` naming nothing here is either another volume's
            // snapshot grafted in, or the record it named having just been deleted, and the page
            // cannot tell those apart — claiming either would be a guess.
            <>
              the snapshot the environment is on (
              <span className="font-mono">{foreignCurrent?.slice(0, 8)}</span>) is no longer in this
              lineage &mdash; changes since are not snapshotted
            </>
          )}
        </div>
        <form action={pushAction} className="mt-2.5 flex flex-wrap items-center gap-2">
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
      </div>
    </Node>
  );

  const pending = pendingNode && (
    <Node>
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm2">Taking a snapshot</div>
          <div className="mt-0.5 text-caption text-muted-foreground">just now · {pusher}</div>
        </div>
        <span className="shrink-0 border border-border px-1.5 py-0.5 text-caption text-muted-foreground">
          uploading…
        </span>
      </div>
    </Node>
  );

  // The live environment (and the snapshot being taken) are NODES under the current record: they
  // are the tip of the branch the environment is on, not records of it.
  const tail = (c: SnapshotNode | null) =>
    c === current && (pending || live) ? (
      <Branch>
        {pending}
        {live}
      </Branch>
    ) : null;

  const render = (c: SnapshotNode): React.ReactNode => {
    const ts = new Date(c.created_at);
    const isCurrent = c === current;
    const kids = childrenOf(c.id);
    return (
      <li key={c.id} className="grid gap-3">
        <ul className="grid gap-3">
          <Node current={isCurrent}>
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                {/* No italics for the fallback: it is the ABSENCE of a message, not a quotation —
                    muted says that. */}
                <div className={`truncate text-sm2 ${c.message ? "" : "text-muted-foreground"}`}>
                  {c.message || "snapshot"}
                </div>
                <div className="mt-0.5 text-caption text-muted-foreground">
                  <span title={ts.toLocaleString("en")}>{when(ts.getTime())}</span> ·{" "}
                  <span className="font-mono">{c.id.slice(0, 8)}</span> · {pusher}
                </div>
              </div>
              <div className="flex shrink-0 items-center gap-1">
                {isCurrent ? (
                  <span className="border border-primary/40 bg-primary/10 px-1.5 py-0.5 text-caption font-medium text-primary">
                    current
                  </span>
                ) : (
                  <RestoreDialog owner={owner} id={id} snapshot={c} envName={envName} current={current} />
                )}
                <DeleteSnapshotDialog owner={owner} id={id} snapshot={c} isCurrent={isCurrent} />
              </div>
            </div>
          </Node>
        </ul>
        {(kids.length > 0 || isCurrent) && (
          <Branch>
            {kids.map(render)}
            {tail(c)}
          </Branch>
        )}
      </li>
    );
  };

  const roots = childrenOf(null);

  return (
    <>
      {/* Only while a push is in flight: the shell's 10 s poll would show a landed snapshot late,
          and this timer vanishes with the last pending node. */}
      {pendingNode && <FastRefresh />}
      <ul className="mt-5 grid gap-3">
        {roots.map(render)}
        {/* Nothing current to hang under: no records yet, or a foreign snapshot on the disk. */}
        {current === null && pending}
        {current === null && live}
        {!envName && history.length === 0 && !pendingNode && (
          <li className="border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
            No snapshots.
          </li>
        )}
      </ul>

      {stale && (
        <p role="alert" className="mt-3 text-caption text-destructive">
          The snapshot has not landed. Refresh, or check the environment&rsquo;s state — a push that
          failed leaves no record.
        </p>
      )}
      <p className="mt-3 text-caption text-muted-foreground">
        Oldest at the top; a snapshot taken after a restore branches off the restored one. The live
        environment sits at the end of its branch — changes since <b>current</b> are not captured
        until you take a snapshot.
      </p>
    </>
  );
}
