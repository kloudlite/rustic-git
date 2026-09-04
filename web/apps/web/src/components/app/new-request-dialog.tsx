"use client";

import { useActionState, useState } from "react";
import { Inbox, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { FieldLabel } from "@/components/auth/auth-card";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { newRequest } from "@/app/(shell)/requests/actions";
import { KINDS, kindLabel, type RequestKind } from "@/lib/requests";
import { DIMS, dimLabel } from "@/lib/quota";
import type { ApiRegion } from "@/lib/api";

/** One picker over the four kinds, driving which field group renders below it — the fields
 *  themselves are `blockFor`'s inputs verbatim, so the two never drift apart. */
export function NewRequestDialog({ owner, regions }: { owner: string; regions: ApiRegion[] }) {
  const [state, action, pending] = useActionState(newRequest, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  const [kind, setKind] = useState<RequestKind>("quota");
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button size="sm"><Inbox />New request</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>New request</DialogTitle>
            <DialogDescription>A superadmin reviews it; nothing changes until it is approved.</DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <div className="grid gap-1">
            <FieldLabel htmlFor="nr-kind">What do you need?</FieldLabel>
            <Select name="kind" value={kind} onValueChange={(v) => setKind(v as RequestKind)}>
              <SelectTrigger id="nr-kind"><SelectValue /></SelectTrigger>
              <SelectContent>
                {KINDS.map((k) => (
                  <SelectItem key={k} value={k}>{kindLabel(k)}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          {kind === "quota" && (
            <div className="grid grid-cols-2 gap-3">
              {DIMS.map((d) => (
                <div key={d} className="grid gap-1">
                  <FieldLabel htmlFor={`nr-${d}`}>{dimLabel(d)}</FieldLabel>
                  <Input id={`nr-${d}`} name={d} type="number" min={0} placeholder="unchanged" className="h-9" />
                </div>
              ))}
            </div>
          )}

          {kind === "access" && (
            <div className="grid gap-3">
              <div className="grid gap-1">
                <FieldLabel htmlFor="nr-team">Team</FieldLabel>
                <Input id="nr-team" name="team" placeholder="acme" />
              </div>
              <div className="grid gap-1">
                <FieldLabel htmlFor="nr-role">Role</FieldLabel>
                <Select name="role" defaultValue="member">
                  <SelectTrigger id="nr-role"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="member">Member</SelectItem>
                    <SelectItem value="admin">Admin</SelectItem>
                    <SelectItem value="owner">Owner</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>
          )}

          {kind === "region" && (
            <div className="grid gap-1">
              <FieldLabel htmlFor="nr-region">Region</FieldLabel>
              <Select name="region">
                <SelectTrigger id="nr-region"><SelectValue placeholder="Pick a region" /></SelectTrigger>
                <SelectContent>
                  {regions.map((r) => (
                    <SelectItem key={r.id} value={r.id}>{r.id}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          )}

          {kind === "other" && (
            <div className="grid gap-3">
              <div className="grid gap-1">
                <FieldLabel htmlFor="nr-title">Title</FieldLabel>
                <Input id="nr-title" name="title" placeholder="What this is about" />
              </div>
              <div className="grid gap-1">
                <FieldLabel htmlFor="nr-body">Description</FieldLabel>
                <Textarea id="nr-body" name="body" placeholder="Details" />
              </div>
            </div>
          )}

          <div className="grid gap-1">
            <FieldLabel htmlFor="nr-reason">Reason</FieldLabel>
            <Textarea id="nr-reason" name="reason" required placeholder="What this is for" />
          </div>
          {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Send request</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
