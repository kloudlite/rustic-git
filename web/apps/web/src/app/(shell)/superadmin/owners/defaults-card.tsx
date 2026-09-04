"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { QuotaReport } from "@/lib/quota";
import { DIMS, dimLabel, type QuotaDim } from "@/lib/quota";
import { writeDefault } from "../actions";

const LABEL: Record<"default-user" | "default-team", string> = {
  "default-user": "Person default",
  "default-team": "Team default",
};

/** One default's row: the display view, or its edit form when `editing`. `fleetMax` is the
 *  current-fleet-max hint per dimension — computed by the page from the owners it already
 *  fetched, not a second round trip. */
function DefaultRow({
  owner,
  quota,
  fleetMax,
}: {
  owner: "default-user" | "default-team";
  quota: QuotaReport | null;
  fleetMax: Record<QuotaDim, number>;
}) {
  const router = useRouter();
  const [editing, setEditing] = useState(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function save(formData: FormData) {
    setPending(true);
    setError(null);
    const r = await writeDefault(owner, formData);
    setPending(false);
    if (!r.ok) {
      setError(r.message);
      return;
    }
    setEditing(false);
    router.refresh();
  }

  const limit = quota?.limit;

  if (!editing) {
    return (
      <div className="flex flex-col gap-2">
        <div className="flex items-center justify-between">
          <span className="text-sm2 font-medium">{LABEL[owner]}</span>
          <Button type="button" size="sm" variant="outline" onClick={() => setEditing(true)}>
            Edit
          </Button>
        </div>
        <div className="flex flex-wrap gap-x-4 gap-y-1 text-caption text-muted-foreground">
          {limit
            ? DIMS.map((d) => (
                <span key={d} className="tabular-nums">
                  {limit[d]} {dimLabel(d).toLowerCase()}
                </span>
              ))
            : "unavailable"}
        </div>
      </div>
    );
  }

  return (
    <form action={save} className="flex flex-col gap-2">
      <span className="text-sm2 font-medium">{LABEL[owner]}</span>
      <div className="grid grid-cols-2 gap-2">
        {DIMS.map((d) => (
          <label key={d} className="flex flex-col gap-0.5 text-caption text-muted-foreground">
            {dimLabel(d)}
            <Input type="number" min={0} name={d} defaultValue={limit?.[d] ?? 0} className="h-8 text-sm2" />
            {/* The upgrade path the brief calls for: an operator lowering this dimension sees, in
                the same breath, the owner already pushing it hardest under the current default. */}
            <span>fleet max {fleetMax[d]}</span>
          </label>
        ))}
      </div>
      <Input name="note" placeholder="Why (required)" required className="h-8 text-sm2" />
      {error && <p role="alert" className="text-sm2 font-medium text-destructive">{error}</p>}
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={pending}>
          Save
        </Button>
        <Button type="button" size="sm" variant="outline" onClick={() => setEditing(false)}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

export function DefaultsCard({
  personDefault,
  teamDefault,
  fleetMax,
}: {
  personDefault: QuotaReport | null;
  teamDefault: QuotaReport | null;
  fleetMax: Record<QuotaDim, number>;
}) {
  return (
    <div className="grid grid-cols-2 gap-6 border border-border bg-card p-4">
      <DefaultRow owner="default-user" quota={personDefault} fleetMax={fleetMax} />
      <DefaultRow owner="default-team" quota={teamDefault} fleetMax={fleetMax} />
    </div>
  );
}
