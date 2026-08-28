"use client";

import Link from "next/link";
import { useActionState, useMemo, useState } from "react";
import { FastRefresh } from "@/components/app/fast-refresh";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { Camera, Check, Copy, Loader2, Package, Play, Plus, Search, Square, SquareTerminal, Terminal, Trash2, Upload } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import type { ApiWorkspace } from "@/lib/api";
import { CopyButton } from "@/components/repo/copy-button";
import { useCopy } from "@/lib/use-copy";
import { sshConfigBlock, sshOneLiner } from "@/lib/ssh-config";
import {
  cloneWorkspace, deleteWorkspace, pushWorkspace, setPackages, startWorkspace, stopWorkspace,
  type WsActionState,
} from "@/app/(shell)/[owner]/(org)/workspaces/actions";

/** Start and stop take one hidden pair of ids and nothing else, so an inline
 *  form (no dialog) does each — same idiom as `pull-actions.tsx`'s bare
 *  `useActionState` forms. Push and clone take an optional value first, so
 *  those two get a small dialog apiece instead. */
function PushDialog({ owner, id }: { owner: string; id: string }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(pushWorkspace, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><Upload />Push</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Push</DialogTitle>
            <DialogDescription>Snapshot and upload the current state as one new entry.</DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={id} />
          <Textarea name="message" placeholder="Message (optional)" rows={3} />
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

function CloneDialog({ owner, id }: { owner: string; id: string }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(cloneWorkspace, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
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

function PackagesDialog({ owner, w }: { owner: string; w: ApiWorkspace }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(setPackages, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><Package />Packages</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Packages</DialogTitle>
            <DialogDescription>
              nixpkgs attribute names, installed into the workspace&rsquo;s profile. Search them at
              search.nixos.org. This replaces the whole list.
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="id" value={w.id} />
          <Input
            name="packages"
            defaultValue={w.packages.join(" ")}
            placeholder="hello jq nodejs_20"
            autoFocus
            className="h-9"
          />
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Apply</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** The declared list, plus what building it produced. No condition yet means the reconciler has
 *  not reported on this list — which is a build in flight, not a failure to hide. */
function Packages({ w }: { w: ApiWorkspace }) {
  if (w.packages.length === 0) return null;
  const st = w.packages_status;
  return (
    <span className="mt-1.5 flex flex-wrap items-center gap-1.5">
      {/* The platform's base set, muted: every workspace has it, and the row's own chips are
          what this workspace ADDS. */}
      {(w.base_packages ?? []).map((p) => (
        <span key={`base-${p}`} className="border border-dashed border-border px-1.5 py-0.5 text-sm2 text-muted-foreground/60" title="base — on every workspace">{p}</span>
      ))}
      {w.packages.map((p) => (
        <span key={p} className="border border-border px-1.5 py-0.5 text-sm2 text-muted-foreground">{p}</span>
      ))}
      {st?.ready ? null : (
        <span
          title={st?.message}
          className={`text-sm2 ${st ? "text-destructive" : "text-muted-foreground"}`}
        >
          {st ? `packages: ${st.reason}` : "installing packages…"}
        </span>
      )}
    </span>
  );
}

/** How to reach this workspace from a terminal — shown only once it HAS an sshd to reach,
 *  which is what `w.ssh` being present means. Both snippets come from `lib/ssh-config.ts`
 *  so what is copied here and what `kl ws ssh-config` writes cannot drift apart. */
function SshDialog({ w }: { w: ApiWorkspace }) {
  const [open, setOpen] = useState(false);
  const { copied, copy } = useCopy();
  const block = sshConfigBlock(w.name, w.id);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><Terminal />SSH</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>SSH to {w.name}</DialogTitle>
          <DialogDescription>
            Sign the CLI in once with <code className="font-mono text-caption">kl login</code>, then:
          </DialogDescription>
        </DialogHeader>

        <div className="flex items-center gap-2 border border-input bg-muted/30 px-3 py-2">
          <code className="min-w-0 flex-1 truncate font-mono text-caption">{sshOneLiner(w.name)}</code>
          <CopyButton value={sshOneLiner(w.name)} label="Copy command" />
        </div>

        {/* No block for a name that cannot legally appear in an ssh config: pasting one would
            put whatever the name contains into the reader's own config. `kl ws ssh` still works
            — it never renders the name. */}
        {block && (
          <p className="text-sm2 text-muted-foreground">
            Or paste a Host block into <code className="font-mono text-caption">~/.ssh/config</code> and
            plain <code className="font-mono text-caption">ssh {w.name}</code> works — the CLI still
            proxies the connection.
          </p>
        )}
        {block && (
          <pre className="overflow-x-auto border border-border bg-muted/30 px-3 py-2 font-mono text-caption">{block}</pre>
        )}

        {block && (
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => copy(block)}>
              {copied ? <Check /> : <Copy />}Copy ssh config
            </Button>
          </DialogFooter>
        )}
      </DialogContent>
    </Dialog>
  );
}

function DeleteDialog({ owner, id, name }: { owner: string; id: string; name: string }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(deleteWorkspace, null);
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
              Stops its container and removes the workspace from this node. Pushed snapshots stay
              in the registry; anything never pushed is gone for good.
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

/** Same filter idiom as `repo-list.tsx`: the whole list is already here, so
 *  filtering it locally is both simpler and faster than a round trip. */
export function WorkspaceList({ owner, workspaces }: { owner: string; workspaces: ApiWorkspace[] }) {
  const [q, setQ] = useState("");
  // A row on its way up lands in one to three seconds; the shell's 10 s poll would show it late.
  const busy = workspaces.some((x) => x.state === "creating" || x.state === "cloning");

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
      {busy && <FastRefresh />}
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
                <Packages w={w} />
              </span>
              <div className="flex shrink-0 items-center gap-2">
                {w.state === "stopped" ? <StartForm owner={owner} id={w.id} /> : <StopForm owner={owner} id={w.id} />}
                {/* A workspace's snapshots are ITS OWNER'S undo history, so this row is the only
                    way to them — they are deliberately absent from the Snapshots tab, which lists
                    the shared artifact (environments). */}
                <Button asChild variant="outline" size="sm">
                  <Link href={`/${owner}/workspaces/${encodeURIComponent(w.id)}/snapshots`}>
                    <Camera />Snapshots
                  </Link>
                </Button>
                {/* Absent until the workspace has a host key — i.e. until there is something
                    to ssh to. A button that only ever errors is worse than no button. */}
                {w.ssh && <SshDialog w={w} />}
                <PackagesDialog owner={owner} w={w} />
                <PushDialog owner={owner} id={w.id} />
                <CloneDialog owner={owner} id={w.id} />
                <DeleteDialog owner={owner} id={w.id} name={w.name} />
              </div>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
