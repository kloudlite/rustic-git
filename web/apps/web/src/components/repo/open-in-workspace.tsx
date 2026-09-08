"use client";

import { useActionState, useEffect, useState } from "react";
import { Loader2, SquareTerminal } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { FieldLabel } from "@/components/auth/auth-card";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { environmentsFor, openInWorkspace, type WsActionState } from "@/app/(shell)/[owner]/(org)/workspaces/actions";
import { QuotaRequestDialog } from "@/components/app/quota-request-dialog";

/** The default name: what the workspace is for, as a slug. Editable — it is a suggestion. */
export function suggestedName(repo: string, branch: string) {
  return `${repo}-${branch}`
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 40);
}

type Env = { id: string; name: string; region: string };

/** One button, two homes: the repo Clone menu and the PR header. Both want the same
 *  thing — a workspace with this repo on this branch — so the dialog lives here once and
 *  the callers only differ in how the button is sized. It asks for the two things the
 *  backend cannot guess: the name, and whether to attach an environment. */
export function OpenInWorkspace({
  owner, repo, branch, label = "Open in a workspace", className, size,
}: {
  owner: string;
  repo: string;
  branch: string;
  label?: string;
  className?: string;
  size?: "sm" | "default";
}) {
  const [state, action, pending] = useActionState<WsActionState, FormData>(openInWorkspace, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  const [envs, setEnvs] = useState<Env[] | null>(null);
  const [environment, setEnvironment] = useState("");
  useEffect(() => {
    if (!open || envs !== null) return;
    let live = true;
    environmentsFor(owner).then((e) => { if (live) setEnvs(e); });
    return () => { live = false; };
  }, [open, envs, owner]);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button type="button" variant="outline" size={size} className={className}><SquareTerminal />{label}</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Open in a workspace</DialogTitle>
            <DialogDescription>
              <span className="font-mono">{owner}/{repo}</span> at <span className="font-mono">{branch}</span>, checked out and ready.
            </DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <input type="hidden" name="repo" value={repo} />
          <input type="hidden" name="branch" value={branch} />
          <div className="grid gap-1">
            <FieldLabel htmlFor="ows-name">Name</FieldLabel>
            <Input id="ows-name" name="name" defaultValue={suggestedName(repo, branch)} required maxLength={40} className="h-9 font-mono" autoFocus />
          </div>
          <div className="grid gap-1">
            <FieldLabel htmlFor="ows-env">Environment</FieldLabel>
            {/* The select's value goes through a hidden input: the form is a server action, and
                a Radix select carries no `name` of its own. */}
            <input type="hidden" name="environment" value={environment} />
            <Select value={environment || "none"} onValueChange={(v) => setEnvironment(v === "none" ? "" : v)}>
              <SelectTrigger id="ows-env" className="h-9">
                <SelectValue placeholder="None" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="none">None</SelectItem>
                {(envs ?? []).map((e) => (
                  <SelectItem key={e.id} value={e.id}>{e.name} <span className="text-muted-foreground">· {e.region}</span></SelectItem>
                ))}
              </SelectContent>
            </Select>
            <p className="text-caption text-muted-foreground">
              {envs === null ? "Loading your environments…" : envs.length === 0 ? "No environments yet; the workspace runs on its own." : "Attached, its services resolve by bare name from the workspace."}
            </p>
          </div>
          {state?.error && (
            <div className="flex flex-wrap items-center gap-2">
              <p role="alert" className="text-caption font-medium text-destructive">{state.error}</p>
              {/* Only when the refusal named a quota dimension — a name-collision 409 gets no
                  request trigger, since raising quota would not fix it. */}
              {state.quotaDim && <QuotaRequestDialog owner={owner} dim={state.quotaDim} />}
            </div>
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>{pending ? <Loader2 className="animate-spin" /> : <SquareTerminal />}Open</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
