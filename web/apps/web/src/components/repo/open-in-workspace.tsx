"use client";

import { useActionState } from "react";
import { Loader2, SquareTerminal } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { FieldLabel } from "@/components/auth/auth-card";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { openInWorkspace, type WsActionState } from "@/app/(shell)/[owner]/(org)/workspaces/actions";
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

/** One button, two homes: the repo Clone menu and the PR header. Both want the same
 *  thing — a workspace with this repo on this branch — so the dialog lives here once and
 *  the callers only differ in how the button is sized. It asks for the two things the
 *  backend cannot guess: the name. The environment its services resolve from is the SPACE's
 *  choice, made once on the environment's page, never per workspace. */
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
