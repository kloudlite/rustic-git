"use client";

import { useActionState } from "react";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { Loader2, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { FieldLabel } from "@/components/auth/auth-card";
import { addSshKey, type AddKeyState } from "@/app/(shell)/settings/actions";
import { OwnerSelect } from "@/components/app/owner-select";
import type { SwitcherOwner } from "@/components/app/team-switcher";

export function AddKeyDialog({
  owners,
  defaultOwner,
  signing = false,
}: {
  owners: SwitcherOwner[];
  defaultOwner: string;
  /** A signing key proves authorship and grants no access — a different thing
   *  from an access key, so it is a different button rather than a checkbox. */
  signing?: boolean;
}) {
  const [state, action, pending] = useActionState<AddKeyState, FormData>(addSshKey, null);
  const [open, setOpen] = useDialogUntilSuccess(state);

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" className="border-edge hover:border-edge-hover"><Plus />{signing ? "Add signing key" : "Add key"}</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <form action={action} className="grid gap-5">
          <DialogHeader>
            <DialogTitle>{signing ? "Add a signing key" : "Add an SSH key"}</DialogTitle>
            <DialogDescription>
              {signing
                ? "An SSH or GPG public key. Commits signed with it show as verified; it grants no access."
                : "Paste the public half. The private key never leaves your machine."}
            </DialogDescription>
          </DialogHeader>
          <div className="grid gap-4 sm:grid-cols-field-pair">
            <div className="grid gap-2">
              <FieldLabel htmlFor="key-title">Title</FieldLabel>
              <Input id="key-title" name="title" placeholder="Work laptop" autoFocus className="h-9" />
            </div>
            {owners.length > 1 && (
              <div className="grid gap-2">
                <FieldLabel htmlFor="key-owner">Namespace</FieldLabel>
                <OwnerSelect id="key-owner" owners={owners} defaultValue={defaultOwner} />
              </div>
            )}
          </div>
          {owners.length < 2 && <OwnerSelect id="key-owner" owners={owners} defaultValue={defaultOwner} />}
          {signing && <input type="hidden" name="signing" value="1" />}
          <div className="grid gap-2">
            <FieldLabel htmlFor="key">Public key</FieldLabel>
            <Textarea
              id="key"
              name="key"
              rows={signing ? 6 : 4}
              spellCheck={false}
              placeholder={signing ? "ssh-ed25519 AAAA… or -----BEGIN PGP PUBLIC KEY BLOCK-----" : "ssh-ed25519 AAAA… you@machine"}
              // A GPG key is ~50 lines, and the shared Textarea grows to fit its
              // content — which pushes the dialog off the screen the moment one is
              // pasted. Capped and scrolled instead: the box stays a box.
              className="max-h-48 resize-y overflow-auto font-mono text-caption"
            />
            <p className="text-caption text-muted-foreground">
              {signing ? (
                <>
                  From <code className="font-mono">~/.ssh/id_ed25519.pub</code>, or{" "}
                  <code className="font-mono">gpg --armor --export you@example.com</code>.
                </>
              ) : (
                <>Usually in <code className="font-mono">~/.ssh/id_ed25519.pub</code>.</>
              )}
            </p>
          </div>
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" className="border-edge hover:border-edge-hover" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Add key</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
