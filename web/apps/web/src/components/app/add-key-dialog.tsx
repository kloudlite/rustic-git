"use client";

import { useActionState, useEffect, useState } from "react";
import { Loader2, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { FieldLabel } from "@/components/auth/auth-card";
import { addSshKey, type AddKeyState } from "@/app/settings/actions";

export function AddKeyDialog() {
  const [open, setOpen] = useState(false);
  const [state, action, pending] = useActionState<AddKeyState, FormData>(addSshKey, null);
  useEffect(() => { if (state?.ok) setOpen(false); }, [state]);

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" className="border-edge hover:border-edge-hover"><Plus />Add key</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <form action={action} className="grid gap-5">
          <DialogHeader>
            <DialogTitle>Add an SSH key</DialogTitle>
            <DialogDescription>Paste the public half. The private key never leaves your machine.</DialogDescription>
          </DialogHeader>
          <div className="grid gap-2">
            <FieldLabel htmlFor="key-title">Title</FieldLabel>
            <Input id="key-title" name="title" placeholder="Work laptop" autoFocus className="h-9" />
          </div>
          <div className="grid gap-2">
            <FieldLabel htmlFor="key">Public key</FieldLabel>
            <Textarea id="key" name="key" rows={4} spellCheck={false} placeholder="ssh-ed25519 AAAA… you@machine" className="resize-y font-mono text-caption" />
            <p className="text-caption text-muted-foreground">Usually in <code className="font-mono">~/.ssh/id_ed25519.pub</code>.</p>
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
