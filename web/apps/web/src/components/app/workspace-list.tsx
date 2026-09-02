"use client";

import Link from "next/link";
import { useActionState, useMemo, useState } from "react";
import { AutoRefresh } from "@/components/app/auto-refresh";
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
import { noticesFor } from "@/lib/ws-status";
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
          <Textarea name="message" placeholder="Message (optional)" aria-label="Message" rows={3} />
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

function ToggleForm({ owner, id, running }: { owner: string; id: string; running: boolean }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(running ? stopWorkspace : startWorkspace, null);
  return (
    <form action={action}>
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="id" value={id} />
      <Button type="submit" variant="outline" size="sm" disabled={pending}>
        {pending ? <Loader2 className="animate-spin" /> : running ? <Square /> : <Play />}
        {running ? "Stop" : "Start"}
      </Button>
      {state?.error && <p role="alert" className="mt-1 text-caption font-medium text-destructive">{state.error}</p>}
      {state?.warning && <p role="status" className="mt-1 text-caption font-medium text-warning">{state.warning}</p>}
    </form>
  );
}

function CloneDialog({ owner, id }: { owner: string; id: string }) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(cloneWorkspace, null);
  // A clone that named the cut it was based on is the one success that must NOT close the dialog:
  // this is the only place that cut is ever named, so hide the `ok` from the auto-close hook.
  const [open, setOpen] = useDialogUntilSuccess(state?.basedOn ? null : state);
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
          <Input name="name" placeholder="Name" aria-label="Name" autoFocus required className="h-9" />
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          {state?.basedOn && <p role="status" className="text-sm2 text-muted-foreground">Cloned — {state.basedOn}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>{state?.basedOn ? "Close" : "Cancel"}</Button>
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
            aria-label="Packages"
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

/** The waiting-on notices: at most one, rendered where the person is already looking for state.
 *  `text-warning` only for the interrupted case — everything else here is information, and a page
 *  where every line is orange is a page nobody reads. Shared with `environment-list.tsx` so the
 *  two lists cannot drift apart on wording. */
export function Notices({ w }: { w: Parameters<typeof noticesFor>[0] }) {
  const notices = noticesFor(w);
  if (notices.length === 0) return null;
  return (
    <>
      {notices.map((n) => (
        <span key={n.text} className={`mt-1 block text-sm2 ${n.tone === "warning" ? "text-warning" : "text-muted-foreground"}`}>
          {n.text}
        </span>
      ))}
    </>
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

        {/* The one-liner is pasted into a SHELL, so it is guarded by the same name rule as the
            config block: a name the api would refuse is never rendered as a command. Such a
            workspace can still be reached by id (`kl ws ssh <id>`). */}
        {block ? (
          <div className="flex items-center gap-2 border border-input bg-muted/30 px-3 py-2">
            <code className="min-w-0 flex-1 truncate font-mono text-caption">{sshOneLiner(w.name)}</code>
            <CopyButton value={sshOneLiner(w.name)} label="Copy command" />
          </div>
        ) : (
          <p className="text-sm2 text-muted-foreground">
            This workspace&rsquo;s name cannot be used in a command; connect with{" "}
            <code className="font-mono text-caption">kl ws ssh {w.id}</code>.
          </p>
        )}

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
  const busy = workspaces.some((x) => x.state === "creating");

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
      {busy && <AutoRefresh intervalMs={2_000} />}
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
                <Notices w={w} />
              </span>
              <div className="flex shrink-0 items-center gap-2">
                <ToggleForm owner={owner} id={w.id} running={w.state !== "stopped"} />
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
