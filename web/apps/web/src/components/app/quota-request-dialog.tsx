"use client";

import { useActionState } from "react";
import { CircleGauge, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { FieldLabel } from "@/components/auth/auth-card";
import { useDialogUntilSuccess } from "@/lib/use-dialog-until-success";
import { requestQuota } from "@/app/(shell)/[owner]/(org)/quota-actions";
import { DIMS, dimLabel, type QuotaDim } from "@/lib/quota";

/** One number field per dimension, all optional — a request is whichever of the six the person
 *  is actually short on, never all six at once. `dim`, when set, is the one a 409 just named, so
 *  the trigger that opens on a refusal lands the cursor on the field that blocked them. */
export function QuotaRequestDialog({ owner, dim }: { owner: string; dim?: QuotaDim | null }) {
  const [state, action, pending] = useActionState(requestQuota, null);
  const [open, setOpen] = useDialogUntilSuccess(state);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm"><CircleGauge />Request quota</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={action} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Request more quota</DialogTitle>
            <DialogDescription>A team admin reviews it; nothing changes until it is approved.</DialogDescription>
          </DialogHeader>
          <input type="hidden" name="owner" value={owner} />
          <div className="grid grid-cols-2 gap-3">
            {DIMS.map((d) => (
              <div key={d} className="grid gap-1">
                <FieldLabel htmlFor={`qr-${d}`}>{dimLabel(d)}</FieldLabel>
                <Input
                  id={`qr-${d}`}
                  name={d}
                  type="number"
                  min={0}
                  placeholder="unchanged"
                  autoFocus={d === dim}
                  className="h-9"
                />
              </div>
            ))}
          </div>
          <div className="grid gap-1">
            <FieldLabel htmlFor="qr-reason">Reason</FieldLabel>
            <Textarea id="qr-reason" name="reason" required placeholder="What the extra room is for" />
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
