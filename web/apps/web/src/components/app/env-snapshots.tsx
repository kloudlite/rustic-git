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

export type SnapshotNode = { id: string; message?: string; created_at: string };

/** How far the dot's centre sits below a card's top edge: the card's `py-3` plus half the first
 *  text line. The rail line is anchored to the same number at both ends, so it starts on the
 *  first dot and stops on the last one. */
const DOT = 22;

/** One node's dot, sitting ON the rail rather than drawing it: the line is a SINGLE element on
 *  the list (see `Rail`), because per-row segments have to meet across a gap they cannot see,
 *  and did not. the dot paints over the line that runs under it. */
function Dot({ variant }: { variant: "head" | "current" | "pending" | "past" | "dim" }) {
  const dot = {
    head: "bg-primary ring-3 ring-primary/25",
    current: "bg-primary ring-3 ring-primary/25",
    pending: "bg-warning",
    past: "bg-muted-foreground/60",
    dim: "border-2 border-muted-foreground/50 bg-card",
  }[variant];
  return (
    <span
      aria-hidden
      className={`absolute left-4 z-20 size-3 rounded-full ${dot}`}
      style={{ top: DOT - 6 }}
    />
  );
}

/** The rail itself: one line, from the first dot to the last. It is a grid item spanning row 1 to
 *  the start of the final row, then reaching `DOT` further with a negative margin — which is how
 *  it ends ON the last dot without anyone measuring a card's height. */
function Rail({ rows }: { rows: number }) {
  if (rows < 2) return null;
  return (
    <span
      aria-hidden
      className="relative z-10 col-start-1 ml-[21px] w-0.5 justify-self-start bg-border"
      style={{ gridRow: `1 / ${rows}`, marginTop: DOT, marginBottom: -DOT }}
    />
  );
}

/** Every node is this card: the rail's gutter on the left, the dot on the line. */
function Node({
  row,
  current,
  children,
}: {
  row: number;
  current?: boolean;
  children: React.ReactNode;
}) {
  return (
    <li
      style={{ gridRow: row, gridColumn: 1 }}
      className={`relative border border-border py-3 pr-4 pl-[42px] ${
        current ? "bg-primary/5" : "bg-card"
      }`}
    >
      {current && <span aria-hidden className="absolute inset-y-0 left-0 w-0.5 bg-primary" />}
      {children}
    </li>
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

  const current = history.find((h) => h.id === currentId) ?? null;
  // A `restoredTo` that names no record here: a restore grafted ANOTHER volume's snapshot in
  // place. Saying so is the honest answer — badging the newest record `current` would claim the
  // environment is on a snapshot it is not.
  const foreignCurrent = currentId !== null && current === null ? currentId : null;
  // Where the environment actually sits. -1 = nothing here is current: archived (no live volume)
  // or a restore that grafted another volume's snapshot. Never restored ⇒ the newest record.
  const currentIndex =
    envName === null || foreignCurrent !== null
      ? -1
      : current
        ? history.indexOf(current)
        : history.length > 0
          ? 0
          : -1;
  const at = currentIndex >= 0 ? history[currentIndex] : null;

  // Explicit grid rows: the rail is a grid item too, and it can only span "first node to last"
  // if nothing is auto-placed around it.
  const headRows = (envName ? 1 : 0) + (pendingNode ? 1 : 0);
  const rows = headRows + history.length;

  return (
    <>
      {/* Only while a push is in flight: the shell's 10 s poll would show a landed snapshot late,
          and this timer vanishes with the last pending node. */}
      {pendingNode && <FastRefresh />}
      <ul className="mt-5 grid gap-3">
        {/* The live environment is a NODE, not a record: it is where the lineage actually is, so
            it heads the rail and carries the action that adds to it. */}
        {envName && (
          <Node row={1}>
            <Dot variant="head" />
            <div className="min-w-0 flex-1">
              <div className="text-sm2 font-medium">Live environment</div>
              <div className="mt-0.5 text-caption text-muted-foreground">
                {history.length === 0 ? (
                  "No snapshots yet — take one to start the lineage"
                ) : at ? (
                  <>
                    changes since{" "}
                    <span className={at.message ? "" : "text-muted-foreground"}>&ldquo;{at.message || "snapshot"}&rdquo;</span>{" "}
                    (<span title={new Date(at.created_at).toLocaleString("en")}>
                      {when(new Date(at.created_at).getTime())}
                    </span>) are not snapshotted
                  </>
                ) : (
                  // Neutral on purpose: `restored_to` naming nothing here is either another
                  // volume's snapshot grafted in, or the record it named having just been deleted,
                  // and the page cannot tell those apart — claiming either would be a guess.
                  <>
                    the snapshot the environment is on (
                    <span className="font-mono">{foreignCurrent?.slice(0, 8)}</span>) is no longer in
                    this lineage &mdash; changes since are not snapshotted
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
                  <p role="alert" className="w-full text-sm2 font-medium text-destructive">
                    {pushState.error}
                  </p>
                )}
              </form>
            </div>
          </Node>
        )}

        {pendingNode && (
          <Node row={envName ? 2 : 1}>
            <Dot variant="pending" />
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
        )}

        {history.map((c, i) => {
          const ts = new Date(c.created_at);
          const isCurrent = i === currentIndex;
          // Above the current node after an in-place restore to an older snapshot: real records the
          // environment is NOT on, and moving to one goes forward, not back. Dimmed so the eye
          // lands on the current marker instead of the top of the list.
          const newer = currentIndex > 0 && i < currentIndex;
          return (
            <Node key={c.id} row={headRows + i + 1} current={isCurrent}>
              <Dot variant={isCurrent ? "current" : newer ? "dim" : "past"} />
              <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                {/* The group label rides INSIDE the first dimmed node, so it cannot separate from
                    the group it names — and the rail runs on unbroken past it. */}
                {newer && i === 0 && (
                  <div className="mb-1 text-caption text-muted-foreground">
                    newer than current — restoring one moves the environment forward
                  </div>
                )}
                {/* No italics for the fallback: it is the ABSENCE of a message, not a quotation —
                    muted says that, and italic only made two adjacent rows disagree in shape. */}
                <div
                  className={`truncate text-sm2 ${
                    newer || !c.message ? "text-muted-foreground" : ""
                  }`}
                >
                  {c.message || "snapshot"}
                </div>
                <div className="mt-0.5 text-caption text-muted-foreground">
                  <span title={ts.toLocaleString("en")}>{when(ts.getTime())}</span> ·{" "}
                  <span className="font-mono">{c.id.slice(0, 8)}</span> · {pusher}
                </div>
                {isCurrent && (
                  <div className="mt-0.5 text-caption font-medium text-primary">↳ environment is here</div>
                )}
              </div>
              <div className="flex shrink-0 items-center gap-1">
                {isCurrent ? (
                  <span className="border border-primary/40 bg-primary/10 px-1.5 py-0.5 text-caption font-medium text-primary">
                    current
                  </span>
                ) : (
                  <RestoreDialog owner={owner} id={id} snapshot={c} envName={envName} current={at ?? history[0]} />
                )}
                <DeleteSnapshotDialog owner={owner} id={id} snapshot={c} isCurrent={isCurrent} />
              </div>
              </div>
            </Node>
          );
        })}

        <Rail rows={rows} />

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
        Newest first. <b>current</b> is the snapshot this environment last landed on; changes since
        it are not captured until you take a snapshot.
      </p>
    </>
  );
}
