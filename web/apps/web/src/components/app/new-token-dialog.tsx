"use client";

import { useActionState, useState } from "react";
import { Check, Copy, KeyRound, Loader2, Plus, TriangleAlert } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { FieldLabel } from "@/components/auth/auth-card";
import { createToken, type CreateTokenState } from "@/app/(shell)/settings/actions";
import { OwnerSelect } from "@/components/app/owner-select";
import type { SwitcherOwner } from "@/components/app/team-switcher";
import { cn } from "@/lib/utils";
import { useCopy } from "@/lib/use-copy";

/** Two steps in one dialog: describe the token, then see it — once. Closing after
 *  the reveal is the only way out, and the value is not shown again. */
export function NewTokenDialog({ owners, defaultOwner }: { owners: SwitcherOwner[]; defaultOwner: string }) {
  const [open, setOpen] = useState(false);
  // The body is remounted on every close. Its action state — the secret — would otherwise
  // outlive the dialog and greet the next open with "Token created" and the same value,
  // which "will not be shown again" had just promised was gone.
  const [gen, setGen] = useState(0);
  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (!o) setGen((g) => g + 1);
      }}
    >
      <DialogTrigger asChild>
        <Button><Plus />Generate token</Button>
      </DialogTrigger>
      <Body key={gen} owners={owners} defaultOwner={defaultOwner} close={() => setOpen(false)} />
    </Dialog>
  );
}

function Body({ owners, defaultOwner, close }: { owners: SwitcherOwner[]; defaultOwner: string; close: () => void }) {
  const { copied, copy } = useCopy();
  const [state, action, pending] = useActionState<CreateTokenState, FormData>(createToken, null);
  const revealed = Boolean(state?.token);
  // Once revealed, Escape and a click outside must not close it either: the dialog is the only
  // place the token will ever be shown, so leaving is a deliberate act — the one button.
  const stayOpen = (e: Event) => { if (revealed) e.preventDefault(); };

  return (
    <DialogContent className="sm:max-w-lg" showCloseButton={!revealed} onEscapeKeyDown={stayOpen} onPointerDownOutside={stayOpen}>
      {!revealed ? (
        <form action={action} className="grid gap-5">
          <DialogHeader>
            <DialogTitle>New personal access token</DialogTitle>
            <DialogDescription>It can clone and push in one namespace, and you will see it once.</DialogDescription>
          </DialogHeader>

          <div className="grid gap-4 sm:grid-cols-field-pair">
            <div className="grid gap-2">
              <FieldLabel htmlFor="tok-name">Name</FieldLabel>
              <Input id="tok-name" name="name" defaultValue={state?.values?.name} placeholder="ci-runner" autoFocus className="h-9" />
            </div>
            {owners.length > 1 && (
              <div className="grid gap-2">
                <FieldLabel htmlFor="tok-owner">Namespace</FieldLabel>
                <OwnerSelect id="tok-owner" owners={owners} defaultValue={state?.values?.owner ?? defaultOwner} />
              </div>
            )}
          </div>
          {owners.length < 2 && <OwnerSelect id="tok-owner" owners={owners} defaultValue={state?.values?.owner ?? defaultOwner} />}

          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}

          <DialogFooter>
            <Button type="button" variant="outline" className="border-edge hover:border-edge-hover" onClick={close}>Cancel</Button>
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
              onClick={() => copy(state?.token ?? "")}
              className={cn("h-full w-10 shrink-0 rounded-none border-l border-input bg-background", copied ? "text-success hover:text-success" : "text-muted-foreground")}>
              {copied ? <Check /> : <Copy />}
            </Button>
          </div>

          <p className="flex items-start gap-2 border border-warning/40 bg-warning/10 px-3 py-2 text-caption text-foreground/90">
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-warning" />
            Treat it like a password. Anyone holding it can clone and push in that namespace.
          </p>

          <DialogFooter>
            <Button type="button" onClick={close}>{copied ? "Done" : "I've copied it"}</Button>
          </DialogFooter>
        </div>
      )}
    </DialogContent>
  );
}
