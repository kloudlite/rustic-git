"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { DIMS, dimLabel, type QuotaDim } from "@/lib/quota";
import { setQuota } from "../../actions";

/** The owner detail page's one dangerous write. Pre-filled with the owner's OWN limit whether it
 *  is a real `Quota` or the default riding through — either way that's the number an operator
 *  edits from. A note is required (Global Constraint: every quota write records why). */
export function SetQuotaForm({ owner, limit }: { owner: string; limit: Record<QuotaDim, number> }) {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(formData: FormData) {
    setPending(true);
    setError(null);
    const r = await setQuota(owner, formData);
    setPending(false);
    if (!r.ok) {
      setError(r.message);
      return;
    }
    setOpen(false);
    router.refresh();
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">Set quota</Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        <form action={submit} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>Set quota for {owner}</DialogTitle>
            <DialogDescription>Replaces this owner&rsquo;s limit on all six dimensions.</DialogDescription>
          </DialogHeader>
          <div className="grid grid-cols-2 gap-3">
            {DIMS.map((d) => (
              <label key={d} className="flex flex-col gap-1 text-caption text-muted-foreground">
                {dimLabel(d)}
                <Input type="number" min={0} name={d} defaultValue={limit[d]} className="h-9" />
              </label>
            ))}
          </div>
          <Input name="note" placeholder="Why (required)" aria-label="Note" required className="h-9" />
          {error && <p role="alert" className="text-sm2 font-medium text-destructive">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Save</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
