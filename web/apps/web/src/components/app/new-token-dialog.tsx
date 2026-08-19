"use client";

import { useActionState, useState } from "react";
import { Check, Copy, KeyRound, Loader2, Plus, TriangleAlert } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { FieldLabel } from "@/components/auth/auth-card";
import { createToken, type CreateTokenState } from "@/app/settings/actions";
import { cn } from "@/lib/utils";

/** Token scopes as a matrix: what a token may touch, and whether it may only look
 *  or also change. */
const SCOPES = [
  { id: "repo", label: "Code repos", read: "Clone and browse", write: "Push, branches, tags" },
  { id: "packages", label: "Package registries", read: "Pull", write: "Publish" },
  { id: "workspaces", label: "Workspaces", read: "View", write: "Open and manage" },
  { id: "environments", label: "Environments", read: "View", write: "Fork, switch, snapshot" },
];

/** Two steps in one dialog: describe the token, then see it — once. Closing after
 *  the reveal is the only way out, and the value is not shown again. */
export function NewTokenDialog() {
  const [open, setOpen] = useState(false);
  const [state, action, pending] = useActionState<CreateTokenState, FormData>(createToken, null);
  const [copied, setCopied] = useState(false);
  const revealed = Boolean(state?.token);


  return (
    <Dialog open={open} onOpenChange={(next) => { setOpen(next); if (!next) setCopied(false); }}>
      <DialogTrigger asChild>
        <Button><Plus />Generate token</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg" showCloseButton={!revealed}>
        {!revealed ? (
          <form action={action} className="grid gap-5">
            <DialogHeader>
              <DialogTitle>New personal access token</DialogTitle>
              <DialogDescription>Give it only the scopes it needs, and an expiry. You will see the token once.</DialogDescription>
            </DialogHeader>

            <div className="grid gap-4 sm:grid-cols-field-pair">
              <div className="grid gap-2">
                <FieldLabel htmlFor="tok-name">Name</FieldLabel>
                <Input id="tok-name" name="name" placeholder="ci-runner" autoFocus className="h-9" />
              </div>
              <div className="grid gap-2">
                <FieldLabel htmlFor="tok-exp">Expires</FieldLabel>
                <Select name="expires" defaultValue="90">
                  <SelectTrigger id="tok-exp" className="h-9 w-full"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="30">30 days</SelectItem>
                    <SelectItem value="90">90 days</SelectItem>
                    <SelectItem value="365">1 year</SelectItem>
                    <SelectItem value="never">Never</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>

            <fieldset>
              <legend className="text-sm2 font-medium leading-none">Scopes</legend>
              <div className="mt-2 border border-border">
                <div className="grid grid-cols-scopes items-center border-b border-border bg-muted/40 px-3 py-1.5 text-micro font-semibold uppercase tracking-label text-muted-foreground">
                  <span>Resource</span><span className="text-center">Read</span><span className="text-center">Write</span>
                </div>
                <ul className="divide-y divide-border">
                  {SCOPES.map((s) => (
                    <li key={s.id} className="grid grid-cols-scopes items-center px-3 py-2">
                      <span className="text-sm2 font-medium">{s.label}</span>
                      <label className="flex cursor-pointer flex-col items-center gap-1 py-0.5">
                        <Checkbox name="scope" value={`${s.id}:read`} aria-label={`${s.label}: read`} />
                        <span className="text-micro text-muted-foreground">{s.read}</span>
                      </label>
                      <label className="flex cursor-pointer flex-col items-center gap-1 py-0.5">
                        <Checkbox name="scope" value={`${s.id}:write`} aria-label={`${s.label}: write`} />
                        <span className="text-micro text-muted-foreground">{s.write}</span>
                      </label>
                    </li>
                  ))}
                </ul>
              </div>
            </fieldset>

            {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}

            <DialogFooter>
              <Button type="button" variant="outline" className="border-edge hover:border-edge-hover" onClick={() => setOpen(false)}>Cancel</Button>
              <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Generate token</Button>
            </DialogFooter>
          </form>
        ) : (
          <div className="grid gap-5">
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2"><KeyRound className="size-4 text-success" /> Token created</DialogTitle>
              <DialogDescription>
                Copy <span className="font-medium text-foreground">{state?.name}</span> now. For your security it will not be shown again.
              </DialogDescription>
            </DialogHeader>

            <div className="flex h-10 items-stretch border border-input bg-muted/30">
              <Input readOnly value={state?.token} onFocus={(e) => e.currentTarget.select()} aria-label="Token"
                className="h-full min-w-0 flex-1 rounded-none border-0 bg-transparent px-3 font-mono text-caption focus-visible:ring-0" />
              <Button type="button" variant="ghost" size="icon" aria-label={copied ? "Copied" : "Copy token"}
                onClick={async () => { await navigator.clipboard.writeText(state!.token!); setCopied(true); }}
                className={cn("h-full w-10 shrink-0 rounded-none border-l border-input bg-background", copied ? "text-success hover:text-success" : "text-muted-foreground")}>
                {copied ? <Check /> : <Copy />}
              </Button>
            </div>

            <p className="flex items-start gap-2 border border-warning/40 bg-warning/10 px-3 py-2 text-caption text-foreground/90">
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-warning" />
              Treat it like a password. Anyone holding it can act as you within these scopes.
            </p>

            <DialogFooter>
              <Button type="button" onClick={() => setOpen(false)}>{copied ? "Done" : "I've copied it"}</Button>
            </DialogFooter>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
