"use client";

import Link from "next/link";
import { useActionState, useMemo, useState } from "react";
import { FastRefresh } from "@/components/app/fast-refresh";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { Camera, Layers, Loader2, Play, Plus, Search, Square, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import { when } from "@/lib/time";
import type { ApiEnvironment } from "@/lib/api";
import {
  cloneEnvironment, deleteEnvironment, deleteEnvironmentSnapshots, startEnvironment, stopEnvironment,
  type EnvActionState,
} from "@/app/(shell)/[owner]/(org)/environments/actions";

/** An environment that no longer exists, but whose snapshots do — a volume on the server tier with
 *  history and no live `Environment`. Restoring one is the whole reason the row is here. */
export type ArchivedEnv = { id: string; name: string; latest_ms: number | null; snapshots: number };

/** Start/stop, same bare-form idiom as `pull-actions.tsx`: one hidden id, no
 *  dialog, since neither action takes a value from the person. */
function ToggleForm({ owner, id, running }: { owner: string; id: string; running: boolean }) {
  const action = running ? stopEnvironment : startEnvironment;
  const [state, act, pending] = useActionState<EnvActionState, FormData>(action, null);
  return (
    <form action={act} className="contents">
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="id" value={id} />
      <Button type="submit" variant="outline" size="sm" disabled={pending} title={state?.error}>
        {pending ? <Loader2 className="animate-spin" /> : running ? <Square /> : <Play />}
        {running ? "Stop" : "Start"}
      </Button>
    </form>
  );
}

/** Same filter idiom as `repo-list.tsx` and `workspace-list.tsx`. */
function DeleteEnvDialog({ owner, id, name }: { owner: string; id: string; name: string }) {
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
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          {/* Default OFF, deliberately: a snapshot is a point in time and outlives the thing it
              was taken of, so the safe outcome of a delete is an archived row you can restore —
              not an irreversible one. A native checkbox; the form reads its presence. */}
          <label className="flex items-start gap-2.5 text-sm2">
            <input type="checkbox" name="snapshots" className="mt-0.5 size-3.5 accent-destructive" />
            <span>
              Also delete its snapshots
              <span className="block text-caption text-muted-foreground">
                Permanent. Without this the environment becomes an archived row you can restore from.
              </span>
            </span>
          </label>
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" variant="destructive" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Delete</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** Same shape as `workspace-list.tsx`'s `CloneDialog` — a name prompt, nothing else. */
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

/** The archived twin of `DeleteEnvDialog`: the environment is already gone, only its snapshots
 *  are left, and this is the one action that removes them. */
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
            <Button type="submit" variant="destructive" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Delete</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** Opens the row's snapshots — live or archived, the same page and the same records. */
function SnapshotsLink({ owner, id }: { owner: string; id: string }) {
  return (
    <Button asChild variant="outline" size="sm">
      <Link href={`/${owner}/environments/${encodeURIComponent(id)}/snapshots`}><Camera />Snapshots</Link>
    </Button>
  );
}

export function EnvironmentList({
  owner,
  environments,
  archived = [],
}: {
  owner: string;
  environments: ApiEnvironment[];
  archived?: ArchivedEnv[];
}) {
  const [q, setQ] = useState("");
  // A row on its way up lands in one to three seconds; the shell's 10 s poll would show it late.
  const busy = environments.some((x) => x.state === "creating" || x.state === "cloning");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return environments;
    return environments.filter((e) => e.name.toLowerCase().includes(needle));
  }, [environments, q]);

  const shownArchived = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return archived;
    return archived.filter((a) => a.name.toLowerCase().includes(needle) || a.id.includes(needle));
  }, [archived, q]);

  if (environments.length === 0 && archived.length === 0) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <Layers className="mx-auto size-6 text-muted-foreground" aria-hidden />
        <p className="mt-3 text-sm2 font-medium">No environments yet</p>
        <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
          An environment runs one or more services, each backed by a volume.
        </p>
      </div>
    );
  }

  return (
    <>
      {busy && <FastRefresh />}
      <div className="relative w-full max-w-xs">
        <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
        <Input
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="Filter environments"
          aria-label="Filter environments"
          className="h-8 pl-8 text-sm2"
        />
      </div>

      {shown.length === 0 && shownArchived.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          Nothing matches that.
        </p>
      ) : (
        <ul className="mt-5 divide-y divide-border border border-border bg-card">
          {shown.map((e) => (
            <li key={e.id} className="flex flex-wrap items-center gap-4 px-5 py-4">
              <span className="min-w-0 flex-1">
                <span className="flex items-center gap-2.5">
                  <span className="truncate text-body font-medium">{e.name}</span>
                  <WsEnvStateBadge state={e.state} />
                </span>
                <span className="mt-1 block text-sm2 text-muted-foreground">
                  {/* Aggregate view mixes personal and team envs — name the owner when it isn't the page's. */}
                  {e.owner !== owner ? `${e.owner} · ` : ""}
                  {e.region} · {e.services.length} {e.services.length === 1 ? "service" : "services"}
                </span>
              </span>
              <div className="flex shrink-0 items-center gap-2">
                <ToggleForm owner={e.owner} id={e.id} running={e.state === "running"} />
                <SnapshotsLink owner={owner} id={e.id} />
                <CloneEnvDialog owner={e.owner} id={e.id} />
                <DeleteEnvDialog owner={e.owner} id={e.id} name={e.name} />
              </div>
            </li>
          ))}
          {/* Archived rows sit in the SAME list, below the live ones: they are environments that
              still exist as data, and separating them into their own page is what made a deleted
              environment's snapshots unreachable in the first place. */}
          {shownArchived.map((a) => (
            <li key={a.id} className="flex flex-wrap items-center gap-4 px-5 py-4">
              <span className="min-w-0 flex-1">
                <span className="flex items-center gap-2.5">
                  <span className="truncate text-body font-medium">{a.name}</span>
                  <span className="shrink-0 border border-border px-1.5 py-0.5 text-caption text-muted-foreground">
                    archived
                  </span>
                </span>
                <span className="mt-1 block text-sm2 text-muted-foreground">
                  {a.snapshots} {a.snapshots === 1 ? "snapshot" : "snapshots"}
                  {a.latest_ms != null && ` · ${when(a.latest_ms)}`}
                </span>
              </span>
              <div className="flex shrink-0 items-center gap-2">
                <SnapshotsLink owner={owner} id={a.id} />
                <DeleteSnapshotsDialog owner={owner} id={a.id} name={a.name} />
              </div>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
