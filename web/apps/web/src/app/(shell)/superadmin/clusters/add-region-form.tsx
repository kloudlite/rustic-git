"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { createRegionAction } from "../actions";

/** Registering a region is a write like any other: a required reason, and the refusal shown here
 *  rather than swallowed — a rejected id or a 403 is exactly what an operator needs to read. */
export function AddRegionForm() {
  const router = useRouter();
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(formData: FormData) {
    setPending(true);
    setError(null);
    const r = await createRegionAction(formData);
    setPending(false);
    if (!r.ok) {
      setError(r.message);
      return;
    }
    router.refresh();
  }

  return (
    <form action={submit} className="border border-border bg-card p-4">
      <div className="flex items-end gap-3">
        <label className="grid gap-1 text-sm2">
          Id
          <Input name="id" required className="h-8" />
        </label>
        <label className="grid gap-1 text-sm2">
          Name
          <Input name="name" required className="h-8" />
        </label>
        <label className="grid flex-1 gap-1 text-sm2">
          Note
          <Input name="note" required placeholder="Why (required)" className="h-8" />
        </label>
        <Button type="submit" size="sm" disabled={pending}>
          {pending && <Loader2 className="animate-spin" />}
          Add region
        </Button>
      </div>
      {error && <p role="alert" className="mt-2 text-sm2 font-medium text-destructive">{error}</p>}
    </form>
  );
}
