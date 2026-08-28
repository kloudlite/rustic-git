"use client";

import { useActionState, useEffect, useState } from "react";
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

/** One node's rail: the dot, plus the line segments above and below it. Splitting the line at the
 *  dot (rather than one stub under it) is what makes the rail read as ONE continuous line through
 *  every node — the first draws nothing above, the last nothing below, so the lineage visibly
 *  begins and ends. The dot sits 11px down: centred on the card's first text line. */
function Rail({
  first,
  last,
  variant,
}: {
  first: boolean;
  last: boolean;
  variant: "head" | "current" | "pending" | "past" | "dim";
}) {
  const dot = {
    head: "bg-primary ring-3 ring-primary/25",
    current: "bg-primary ring-3 ring-primary/25",
    pending: "bg-warning",
    past: "bg-muted-foreground/50",
    dim: "border border-muted-foreground/50",
  }[variant];
  return (
    <span className="relative flex w-3 shrink-0 self-stretch justify-center" aria-hidden>
      {!first && <span className="absolute top-0 h-[11px] w-0.5 bg-border" />}
      {!last && <span className="absolute top-[11px] bottom-0 w-0.5 bg-border" />}
      <span className={`relative mt-1.5 size-2.5 shrink-0 rounded-full ${dot}`} />
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

  return (
    <>
      {/* Only while a push is in flight: the shell's 10 s poll would show a landed snapshot late,
          and this timer vanishes with the last pending node. */}
      {pendingNode && <FastRefresh />}
      <ul className="mt-5 border border-border bg-card">
        {/* The live environment is a NODE, not a record: it is where the lineage actually is, so
            it heads the rail and carries the action that adds to it. */}
        {envName && (
          <li className="flex items-start gap-3.5 px-5 py-3.5">
            <Rail first last={history.length === 0 && !pendingNode} variant="head" />
            <div className="min-w-0 flex-1">
              <div className="text-sm2 font-medium">Live environment</div>
              <div className="mt-0.5 text-caption text-muted-foreground">
                {history.length === 0 ? (
                  "No snapshots yet — take one to start the lineage"
                ) : at ? (
                  <>
                    changes since{" "}
                    <span className={at.message ? "" : "italic"}>
                      &ldquo;{at.message || "snapshot"}&rdquo;
                    </span>{" "}
                    (<span title={new Date(at.created_at).toLocaleString("en")}>
                      {when(new Date(at.created_at).getTime())}
                    </span>) are not snapshotted
                  </>
                ) : (
                  <>
                    restored from another volume&rsquo;s snapshot{" "}
                    <span className="font-mono">{foreignCurrent?.slice(0, 8)}</span> — changes since are
                    not snapshotted
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
          </li>
        )}

        {pendingNode && (
          <li className="flex items-start gap-3.5 px-5 py-3.5">
            <Rail first={!envName} last={history.length === 0} variant="pending" />
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
          const ts = new Date(c.created_at);
          const isCurrent = i === currentIndex;
          // Above the current node after an in-place restore to an older snapshot: real records the
          // environment is NOT on, and moving to one goes forward, not back. Dimmed so the eye
          // lands on the current marker instead of the top of the list.
          const newer = currentIndex > 0 && i < currentIndex;
          return (
            <li
              key={c.id}
              className={`flex items-start gap-3.5 py-3.5 pr-5 ${
                isCurrent ? "border-l-2 border-l-primary bg-primary/5 pl-[calc(1.25rem-2px)]" : "pl-5"
              }`}
            >
              <Rail
                first={false}
                last={i === history.length - 1}
                variant={isCurrent ? "current" : newer ? "dim" : "past"}
              />
              <div className="min-w-0 flex-1">
                {/* The group label rides INSIDE the first dimmed node, so it cannot separate from
                    the group it names — and the rail runs on unbroken past it. */}
                {newer && i === 0 && (
                  <div className="mb-1 text-caption text-muted-foreground">
                    newer than current — restoring one moves the environment forward
                  </div>
                )}
                <div
                  className={`truncate text-sm2 ${c.message ? "" : "italic"} ${
                    newer ? "text-muted-foreground" : c.message ? "" : "text-muted-foreground"
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
              {isCurrent ? (
                <span className="shrink-0 border border-primary/40 bg-primary/10 px-1.5 py-0.5 text-caption font-medium text-primary">
                  current
                </span>
              ) : (
                <RestoreDialog owner={owner} id={id} snapshot={c} envName={envName} current={at ?? history[0]} />
              )}
            </li>
          );
        })}

        {!envName && history.length === 0 && !pendingNode && (
          <li className="px-5 py-12 text-center text-sm2 text-muted-foreground">No snapshots.</li>
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
