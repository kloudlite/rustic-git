"use client";

import { useActionState, useEffect, useState } from "react";
import { Camera, Loader2, RotateCcw, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { stamp, when } from "@/lib/time";
import { pendingPush } from "@/lib/pending-push";
import { snapshotTime } from "@/lib/snapshot";
import { stateSummary, type SnapshotState } from "@/lib/snapshot-state";
import { deleteVolumeCopy } from "@/lib/archived";
import { envCurrent } from "@/lib/env-current";
import {
  deleteEnvironmentSnapshot, pushEnvironment, restoreEnvironmentFrom, type EnvActionState,
} from "@/app/(shell)/[owner]/(org)/environments/actions";

export type SnapshotNode = { id: string; message?: string; createdAt: string | null; parent: string | null; state?: SnapshotState | null };

/** The rail's geometry, lifted from the landing page's environment panel so the two read as one
 *  drawing: the main lane 27px in, a branch lane every 18px further, a 12px ring on the lane. */
const LANE0 = 27;
const LANE = 18;
const RING = 6;

type Row = {
  key: string;
  lane: number;
  /** Lanes running straight through this row. */
  through: number[];
  /** Lanes that end on this row: a line from the top down to the node. */
  ends: number[];
  /** Lanes that start on this row: a line from the node down to the bottom. */
  starts: number[];
  /** A branch lane whose oldest record sits ABOVE this row and whose parent is this row's node:
   *  the line comes down that lane and elbows into the node here. */
  joins: number[];
};

const laneX = (l: number) => LANE0 + l * LANE;

/** The rail beside one row: absolutely positioned so it is exactly the row's height, whatever
 *  the row holds. Lines and elbows are drawn to the row's vertical middle, where the ring sits. */
function Rail({ row, lanes, variant }: { row: Row; lanes: number; variant: "live" | "current" | "pending" | "past" }) {
  const x = laneX(row.lane);
  const stroke = (l: number) => (l === 0 ? "stroke-primary/40" : "stroke-border");
  return (
    <svg aria-hidden className="pointer-events-none absolute inset-y-0 left-0 h-full" style={{ width: laneX(lanes) }}>
      {row.through.map((l) => (
        <line key={`t${l}`} x1={laneX(l)} x2={laneX(l)} y1="0" y2="100%" className={stroke(l)} strokeWidth="1.5" />
      ))}
      {row.ends.map((l) => (
        <line key={`e${l}`} x1={laneX(l)} x2={laneX(l)} y1="0" y2="50%" className={stroke(l)} strokeWidth="1.5" />
      ))}
      {row.starts.map((l) => (
        <line key={`s${l}`} x1={laneX(l)} x2={laneX(l)} y1="50%" y2="100%" className={stroke(l)} strokeWidth="1.5" />
      ))}
      {row.joins.map((l) => (
        <g key={`j${l}`} className="stroke-border" strokeWidth="1.5" fill="none">
          <line x1={laneX(l)} x2={laneX(l)} y1="0" y2="50%" />
          <line x1={laneX(l)} x2={x + RING} y1="50%" y2="50%" />
        </g>
      ))}
      {variant === "live" ? (
        <circle cx={x} cy="50%" r={RING} className="fill-card stroke-primary" strokeWidth="2" />
      ) : variant === "current" ? (
        <circle cx={x} cy="50%" r={RING} className="fill-primary" />
      ) : variant === "pending" ? (
        <circle cx={x} cy="50%" r={RING} className="fill-card stroke-warning" strokeWidth="2" />
      ) : (
        <circle cx={x} cy="50%" r={RING} className="fill-card stroke-muted-foreground/50" strokeWidth="2" />
      )}
    </svg>
  );
}

function Node({
  row,
  lanes,
  variant,
  children,
}: {
  row: Row;
  lanes: number;
  variant: "live" | "current" | "pending" | "past";
  children: React.ReactNode;
}) {
  return (
    <li
      className={`relative flex items-center gap-3 border-t border-border py-3 pr-4 first:border-t-0 ${
        variant === "live" || variant === "current" ? "bg-primary/5" : ""
      }`}
      style={{ paddingLeft: laneX(lanes) + 4 }}
    >
      <Rail row={row} lanes={lanes} variant={variant} />
      <div className="min-w-0 flex-1">{children}</div>
    </li>
  );
}

/** Restore, in the one dialog both shapes share.
 *
 *  Live: always IN PLACE — there is nothing to name, and the one thing the dialog has to make
 *  unmissable is that the changes since the current snapshot are discarded. Keeping them is the
 *  minor option: closed until asked for, and it opens a message field for the safety snapshot
 *  the restore then waits on, so "you can come back to it" is true rather than merely offered.
 *
 *  Archived: nothing to discard and nothing to restore into, so it asks for a name instead. */
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
  const [keep, setKeep] = useState(false);
  const label = snapshot.message || "snapshot";
  const since = current
    ? `\u201c${current.message || "snapshot"}\u201d (${when(snapshotTime(current))})`
    : null;
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
              {envName ? (
                <>The environment&rsquo;s services stop, its data is replaced with this snapshot, and they start again.</>
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
          {/* The dialog's own choice, stated outright: the action never infers it from a name. */}
          <input type="hidden" name="mode" value={envName ? "inplace" : "new"} />
          {envName ? (
            <>
              <p
                role="alert"
                className="border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm2 font-medium text-destructive"
              >
                {since
                  ? `Every change made since ${since} will be discarded. This cannot be undone.`
                  : "Every change made since the last snapshot will be discarded. This cannot be undone."}
              </p>
              <div className="text-caption">
                {keep ? (
                  <div className="grid gap-1.5">
                    <input type="hidden" name="snapshotFirst" value="1" />
                    <label htmlFor={`restore-keep-${snapshot.id}`} className="text-muted-foreground">
                      The current state is snapshotted first, so you can come back to it. The restore
                      waits for that snapshot to land.
                    </label>
                    <Input
                      id={`restore-keep-${snapshot.id}`}
                      name="snapshotMessage"
                      defaultValue={`before restore to ${label}`}
                      placeholder="Message for the snapshot"
                      autoFocus
                      className="h-9"
                    />
                  </div>
                ) : (
                  <button
                    type="button"
                    onClick={() => setKeep(true)}
                    className="font-medium text-primary underline-offset-4 hover:underline"
                  >
                    Keep the current state first?
                  </button>
                )}
              </div>
            </>
          ) : (
            <Input
              name="name"
              placeholder="Name"
              autoFocus
              required
              className="h-9"
              aria-label="Restored environment name"
            />
          )}
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>
              {pending && <Loader2 className="animate-spin" />}Restore
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** Delete ONE snapshot — the explicit delete a snapshot is kept until, bytes included. The live
 *  environment is not affected; the current node says the one extra thing that IS lost, which is
 *  the lineage still showing where the environment sits. */
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
              {deleteVolumeCopy(1)} The environment itself is not affected.
              {isCurrent && (
                <>
                  {" "}
                  This is the snapshot the environment currently sits on; its disk does not change,
                  but the snapshots will no longer show where it is.
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
  const [pushState, dispatchPush, pushing] = useActionState<EnvActionState, FormData>(pushEnvironment, null);
  const [asked, setAsked] = useState<{ request: string; had: number } | null>(null);
  const [stale, setStale] = useState(false);
  // `had` is the length at SUBMIT, not at the render carrying the result: the action already
  // revalidates the page, so a fast push lands in the same render its request id arrives in, and
  // a length read then would count the new record and wait for one more that never comes.
  const [hadAtSubmit, setHadAtSubmit] = useState(0);
  const pushAction = (fd: FormData) => {
    setHadAtSubmit(history.length);
    dispatchPush(fd);
  };
  if (pushState?.requestId && asked?.request !== pushState.requestId) {
    setAsked({ request: pushState.requestId, had: hadAtSubmit });
    setStale(false);
  }
  // Cleared the moment the record lands and never re-derived from the length afterwards: a
  // history that later shrinks (a deleted record) must not put a landed push back into
  // "uploading…" and, five minutes on, into a false "has not landed".
  if (asked && !pendingPush(asked, history.length)) setAsked(null);
  const waiting = asked !== null;
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
  // Full ancestry, no restore cutoff — used below only to lay out the rail (which branch is
  // "on the way to" current), not to decide what current IS (that's `envCurrent`).
  const descends = (n: SnapshotNode, anc: string): boolean => {
    for (let p: SnapshotNode | undefined = n; p; p = p.parent ? byId.get(p.parent) : undefined) {
      if (p.id === anc) return true;
    }
    return false;
  };

  // One rule, shared with the environment header — see `lib/env-current.ts`.
  const { current, foreign: foreignCurrent } = envCurrent(history, {
    live: envName !== null,
    restoredTo,
    restoredAt,
  });

  // Oldest first, and the branch the environment is on LAST among siblings, so the live node is
  // the bottom of the tree rather than buried between two branches.
  const childrenOf = (parent: string | null) =>
    history
      .filter((h) => (h.parent && byId.has(h.parent) ? h.parent : null) === parent)
      .sort((a, b) => {
        const onPath = (n: SnapshotNode) => (current && descends(current, n.id) ? 1 : 0);
        return onPath(a) - onPath(b) || snapshotTime(a) - snapshotTime(b);
      });

  // Flatten the tree into rows, NEWEST first like a log, assigning lanes: the branch the
  // environment is on is lane 0 all the way down, every other branch forks onto a fresh lane. The
  // live environment (and a snapshot being taken) head the list on lane 0 — the reference is the
  // landing page's panel, where "you" sits at the top and the rail runs down from it.
  type Flat = { kind: "record" | "pending" | "live"; node: SnapshotNode | null; lane: number };
  const oldestFirst: Flat[] = [];
  let lanesUsed = 0;
  const walk = (n: SnapshotNode, lane: number) => {
    oldestFirst.push({ kind: "record", node: n, lane });
    const kids = childrenOf(n.id);
    kids.forEach((k, i) => walk(k, i === kids.length - 1 ? lane : ++lanesUsed));
  };
  childrenOf(null).forEach((r, i, all) => walk(r, i === all.length - 1 ? 0 : ++lanesUsed));
  const flat: Flat[] = [...oldestFirst].reverse();
  const headLane = current ? (flat.find((f) => f.node === current)?.lane ?? 0) : 0;
  if (pendingNode) flat.unshift({ kind: "pending", node: null, lane: headLane });
  if (envName) flat.unshift({ kind: "live", node: null, lane: headLane });
  const lanes = lanesUsed + 1;
  const first = new Map<number, number>();
  const last = new Map<number, number>();
  flat.forEach((f, i) => {
    if (!first.has(f.lane)) first.set(f.lane, i);
    last.set(f.lane, i);
  });
  // A lane runs from the first row it appears on (its newest) to its last (its oldest); below
  // that its oldest record's parent sits on another lane, and the line keeps going down to that
  // parent's row, where it elbows in. Reading newest-first, that is a branch line dropping down
  // into the record it forked from.
  const rows: Row[] = flat.map((f, i) => {
    const through: number[] = [];
    const ends: number[] = [];
    const starts: number[] = [];
    const joins: number[] = [];
    for (let l = 0; l < lanes; l++) {
      const a = first.get(l) ?? -1;
      const z = last.get(l) ?? -1;
      if (a < 0) continue;
      const oldest = flat[z].node;
      const parentRow = oldest?.parent ? flat.findIndex((g) => g.node?.id === oldest.parent) : -1;
      const tail = parentRow > z ? parentRow : z;
      if (i === parentRow && parentRow > z && l !== f.lane) joins.push(l);
      else if (a < i && i < tail) through.push(l);
      else if (i === a && i < tail) starts.push(l);
      else if (i === z && a < i) ends.push(l);
    }
    return { key: f.node?.id ?? f.kind, lane: f.lane, through, ends, starts, joins };
  });

  return (
    <>
      {/* Only while a push is in flight: the shell's 10 s poll would show a landed snapshot late,
          and this timer vanishes with the last pending node. */}
      {pendingNode && <AutoRefresh intervalMs={2_000} />}
      <ul className="mt-5 border border-border bg-card">
        {flat.map((f, i) => {
          const row = rows[i];
          if (f.kind === "live") {
            return (
              <Node key={row.key} row={row} lanes={lanes} variant="live">
                <div className="flex items-center gap-2.5">
                  <span className="text-sm2 font-medium">Live environment</span>
                  <span className="inline-flex items-center gap-1.5 border border-success/40 bg-success/10 px-2 py-0.5 font-mono text-caption text-success">
                    <span className="size-1.5 rounded-full bg-success" />live
                  </span>
                </div>
                <div className="mt-0.5 text-caption text-muted-foreground">
                  {history.length === 0 ? (
                    "No snapshots yet — take one to start"
                  ) : current ? (
                    <>
                      changes since <span>&ldquo;{current.message || "snapshot"}&rdquo;</span> (
                      <span title={stamp(snapshotTime(current))}>
                        {when(snapshotTime(current))}
                      </span>
                      ) are not snapshotted
                    </>
                  ) : (
                    // Neutral on purpose: `restored_to` naming nothing here is either another
                    // volume's snapshot grafted in, or the record it named having just been
                    // deleted, and the page cannot tell those apart.
                    <>
                      the snapshot the environment is on (
                      <span className="font-mono">{foreignCurrent?.slice(0, 8)}</span>) is no longer in
                      this environment&rsquo;s snapshots &mdash; changes since are not snapshotted
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
              </Node>
            );
          }
          if (f.kind === "pending") {
            return (
              <Node key={row.key} row={row} lanes={lanes} variant="pending">
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
          }
          const c = f.node!;
          const ts = new Date(snapshotTime(c));
          const isCurrent = c === current;
          return (
            <Node key={row.key} row={row} lanes={lanes} variant={isCurrent ? "current" : "past"}>
              <div className="flex items-start gap-3">
                <div className="min-w-0 flex-1">
                  {/* No italics for the fallback: it is the ABSENCE of a message, not a quotation —
                      muted says that. */}
                  <div className={`truncate text-sm2 ${c.message ? "" : "text-muted-foreground"}`}>
                    {c.message || "snapshot"}
                  </div>
                  {stateSummary(c.state) && (
                    <div className="mt-0.5 text-sm2 text-muted-foreground">{stateSummary(c.state)}</div>
                  )}
                  <div className="mt-0.5 text-caption text-muted-foreground">
                    <span title={stamp(ts.getTime())}>{when(ts.getTime())}</span> ·{" "}
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
          );
        })}
        {!envName && history.length === 0 && !pendingNode && (
          <li className="px-5 py-12 text-center text-sm2 text-muted-foreground">
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
        Newest first. <b>current</b> is the snapshot the environment sits on; a snapshot taken after
        a restore branches off the restored one, and the live environment carries what has changed
        since <b>current</b> until you take a snapshot.
      </p>
    </>
  );
}
