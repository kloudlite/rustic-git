"use client";

import Link from "next/link";
import { useActionState, useEffect, useState } from "react";
import { ChevronRight, Loader2, RotateCcw, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { when } from "@/lib/time";
import { snapshotTime } from "@/lib/snapshot";
import { stateSummary } from "@/lib/snapshot-state";
import { deleteVolumeCopy, type ArchivedRow } from "@/lib/archived";
import type { ApiCommitRecord } from "@/lib/api";
import { volumeSnapshots } from "@/app/(shell)/[owner]/(org)/volume-actions";
import { deleteWorkspaceSnapshots, restoreWorkspace } from "@/app/(shell)/[owner]/(org)/workspaces/actions";
import {
  deleteEnvironmentSnapshots, restoreEnvironmentFrom,
} from "@/app/(shell)/[owner]/(org)/environments/actions";

/** Both kinds' action states are `{ ok?, error? }` plus fields this section never reads, so one
 *  shape drives both dialogs rather than the section being written twice. */
type DialogState = { ok?: true; error?: string } | null;
type DialogAction = (prev: DialogState, fd: FormData) => Promise<DialogState>;

export type ArchivedKind = "workspace" | "environment";

/** Restore, from a row whose workspace/environment no longer exists.
 *
 *  A snapshot is picked HERE rather than on a page of its own: the row is one line in a collapsed
 *  section, and the person's question is "which point do I want back", which needs the list. The
 *  list is fetched when the dialog OPENS — a closed row costs nothing — and the fields below it
 *  are pre-filled from the chosen snapshot's own frozen definition, so restoring an old snapshot
 *  brings back the image and packages that snapshot had, not today's. */
function RestoreDialog({ owner, kind, row }: { owner: string; kind: ArchivedKind; row: ArchivedRow }) {
  const act: DialogAction = kind === "workspace" ? restoreWorkspace : restoreEnvironmentFrom;
  const [state, action, pending] = useActionState<DialogState, FormData>(act, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  // Three states, not two: not read yet, failed, and genuinely empty. A failed read that renders
  // as "no snapshots" is a false claim of data loss.
  const [snaps, setSnaps] = useState<ApiCommitRecord[] | null>(null);
  const [snapsError, setSnapsError] = useState<string | null>(null);
  const [sel, setSel] = useState("");

  useEffect(() => {
    if (!open || snaps !== null || snapsError !== null) return;
    let live = true;
    volumeSnapshots(row.id).then((r) => {
      if (!live) return;
      if (!r.ok) {
        setSnapsError(r.error);
        return;
      }
      setSnaps(r.rows);
      // Newest first from the api, and the newest is what a restore almost always means.
      setSel(r.rows[0]?.id ?? "");
    });
    return () => {
      live = false;
    };
  }, [open, snaps, snapsError, row.id]);

  const chosen = snaps?.find((s) => s.id === sel) ?? null;
  // Only a workspace definition pre-fills fields: an environment's is its service list, which
  // this dialog does not build — omitting `services` restores exactly what the push froze.
  const def = chosen?.state?.kind === "workspace" ? chosen.state : null;

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><RotateCcw />Restore</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Restore {row.name}</DialogTitle>
            <DialogDescription>
              A new {kind}, holding the chosen snapshot&rsquo;s data and the definition that
              snapshot froze.
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={row.id} />
          {/* `restoreEnvironmentFrom` never infers where to restore to; there is no environment
              left to restore INTO, so this one is always new. Ignored by the workspace action. */}
          <input type="hidden" name="mode" value="new" />
          <input type="hidden" name="snapshotId" value={sel} />

          <div className="grid gap-1.5">
            <label htmlFor={`snap-${row.id}`} className="text-sm2 font-medium">Snapshot</label>
            {snapsError !== null ? (
              <p role="alert" className="text-sm2 font-medium text-destructive">
                Could not read the snapshots — try again. ({snapsError})
              </p>
            ) : snaps === null ? (
              <p className="text-sm2 text-muted-foreground">Reading the snapshots…</p>
            ) : snaps.length === 0 ? (
              <p role="alert" className="text-sm2 text-muted-foreground">
                No snapshots left to restore from.
              </p>
            ) : (
              <select
                id={`snap-${row.id}`}
                value={sel}
                onChange={(e) => setSel(e.target.value)}
                className="h-9 border border-border bg-card px-2 text-sm2"
              >
                {snaps.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.message || c.id.slice(0, 8)} — {when(snapshotTime(c))}
                  </option>
                ))}
              </select>
            )}
            {chosen && stateSummary(chosen.state) && (
              <p className="text-caption text-muted-foreground">{stateSummary(chosen.state)}</p>
            )}
          </div>

          <Input name="name" placeholder="Name" aria-label="Name" required className="h-9" />
          {def && (
            <div className="grid gap-2">
              <Input
                name="image"
                aria-label="Image"
                key={`i${sel}`}
                defaultValue={def.image}
                placeholder="Image"
                className="h-9"
              />
              <Input
                name="packages"
                aria-label="Packages"
                key={`p${sel}`}
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
            <Button type="submit" disabled={pending || !sel}>
              {pending && <Loader2 className="animate-spin" />}Restore
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** The one irreversible action in this section. Its snapshots are all that is keeping the volume,
 *  so deleting them deletes it — the copy counts them rather than saying "everything". */
function DeleteVolumeDialog({ owner, kind, row }: { owner: string; kind: ArchivedKind; row: ArchivedRow }) {
  const act: DialogAction = kind === "workspace" ? deleteWorkspaceSnapshots : deleteEnvironmentSnapshots;
  const [state, action, pending] = useActionState<DialogState, FormData>(act, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="ghost" size="sm" className="text-destructive" aria-label={`Delete volume ${row.name}`}>
          <Trash2 />
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Delete {row.name}?</DialogTitle>
            <DialogDescription>{deleteVolumeCopy(row.snapshots)}</DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={row.id} />
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

/** The Snapshots section, one per kind, at the bottom of that kind's list.
 *
 *  Collapsed and native `<details>`, so the disclosure works before hydration and needs no state:
 *  these are the things that are GONE, kept only by their snapshots, and the working set above is
 *  what the page is for. The row's name links to that volume's own snapshots page — the same page
 *  a live one has — because the two actions here are the common ones, not all of them. */
export function ArchivedSnapshots({
  owner,
  kind,
  rows,
}: {
  owner: string;
  kind: ArchivedKind;
  rows: ArchivedRow[];
}) {
  if (rows.length === 0) return null;
  const base = `/${owner}/${kind === "workspace" ? "workspaces" : "environments"}`;
  return (
    <details className="mt-7 group">
      <summary className="flex cursor-pointer list-none items-center gap-2 text-caption font-semibold tracking-wider text-muted-foreground uppercase">
        <ChevronRight className="size-3.5 transition-transform group-open:rotate-90" aria-hidden />
        Snapshots ({rows.length})
        <span className="text-caption font-normal tracking-normal normal-case">
          — {kind}s that are gone; their snapshots are not
        </span>
      </summary>
      <ul className="mt-2.5 divide-y divide-border border border-border bg-card">
        {rows.map((r) => (
          <li key={r.id} className="flex items-center gap-3.5 px-5 py-3.5">
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2.5">
                <Link
                  href={`${base}/${encodeURIComponent(r.id)}/snapshots`}
                  className="truncate text-body font-medium underline-offset-4 hover:underline"
                >
                  {r.name}
                </Link>
                <span className="shrink-0 border border-border px-1.5 py-0.5 text-caption text-muted-foreground">
                  deleted
                </span>
              </div>
              <span className="mt-1 block text-sm2 text-muted-foreground">
                {r.snapshots} {r.snapshots === 1 ? "snapshot" : "snapshots"}
                {r.lastPushAt && ` · last push ${when(Date.parse(r.lastPushAt))}`}
                {!r.named && " · name not recorded"}
              </span>
            </div>
            <div className="flex shrink-0 items-center gap-1">
              <RestoreDialog owner={owner} kind={kind} row={r} />
              <DeleteVolumeDialog owner={owner} kind={kind} row={r} />
            </div>
          </li>
        ))}
      </ul>
    </details>
  );
}
