"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { QuotaReport } from "@/lib/quota";
import { DIMS, dimLabel, dimUnit, type QuotaDim } from "@/lib/quota";
import { writeDefault } from "../actions";

type DefaultOwner = "default-user" | "default-team";

const COLUMNS: { owner: DefaultOwner; label: string; hint: string }[] = [
  { owner: "default-user", label: "Person", hint: "an owner with no quota of their own" },
  { owner: "default-team", label: "Team", hint: "a team with no quota of its own" },
];

/** The two defaults as one table — dimensions down, person and team across — so the two
 *  columns read against each other, which is the only question this card answers ("is the
 *  team default really 4× the person one?"). Editing turns one column into inputs in place;
 *  the other column stays readable so the operator keeps the comparison while typing. */
export function DefaultsCard({
  personDefault,
  teamDefault,
  fleetMax,
}: {
  personDefault: QuotaReport | null;
  teamDefault: QuotaReport | null;
  fleetMax: Record<QuotaDim, number>;
}) {
  const router = useRouter();
  const [editing, setEditing] = useState<DefaultOwner | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const limits: Record<DefaultOwner, Record<QuotaDim, number> | undefined> = {
    "default-user": personDefault?.limit,
    "default-team": teamDefault?.limit,
  };

  async function save(formData: FormData) {
    if (!editing) return;
    setPending(true);
    setError(null);
    const r = await writeDefault(editing, formData);
    setPending(false);
    if (!r.ok) {
      setError(r.message);
      return;
    }
    setEditing(null);
    router.refresh();
  }

  function cancel() {
    setEditing(null);
    setError(null);
  }

  return (
    <form action={save} className="border border-border bg-card">
      <div className="flex items-baseline justify-between border-b border-border px-4 py-3">
        <div>
          <h2 className="text-sm2 font-medium">Defaults</h2>
          <p className="text-caption text-muted-foreground">
            What an owner gets until they have a quota of their own. Changing a default changes it
            for everyone still on it.
          </p>
        </div>
      </div>
      <div className="overflow-x-auto">
        <table className="w-full text-sm2">
          <thead>
            <tr className="border-b border-border text-caption uppercase tracking-eyebrow text-muted-foreground">
              <th className="px-4 py-2 text-left font-medium">Dimension</th>
              {COLUMNS.map((c) => (
                <th key={c.owner} className="px-4 py-2 text-right font-medium">
                  <span className="inline-flex items-center gap-3">
                    <span title={c.hint}>{c.label}</span>
                    {editing === c.owner ? null : (
                      <Button
                        type="button"
                        size="sm"
                        variant="outline"
                        className="h-6 px-2 normal-case tracking-normal"
                        disabled={editing !== null || !limits[c.owner]}
                        onClick={() => setEditing(c.owner)}
                      >
                        Edit
                      </Button>
                    )}
                  </span>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {DIMS.map((d) => (
              <tr key={d} className="border-b border-border last:border-b-0">
                <td className="px-4 py-2 text-muted-foreground">
                  {dimLabel(d)}
                  {dimUnit(d) && <span className="ml-1 text-caption">({dimUnit(d)})</span>}
                </td>
                {COLUMNS.map((c) => {
                  const limit = limits[c.owner];
                  if (editing === c.owner) {
                    return (
                      <td key={c.owner} className="px-4 py-1.5 text-right">
                        <div className="ml-auto flex w-40 flex-col items-end gap-0.5">
                          <Input
                            type="number"
                            min={0}
                            name={d}
                            defaultValue={limit?.[d] ?? 0}
                            className="h-8 w-full text-right text-sm2 tabular-nums"
                          />
                          {/* An operator lowering a default sees, in the same cell, the owner
                              already pushing it hardest — a default below that number strands
                              someone's live objects over quota. */}
                          <span
                            className={
                              "text-caption " +
                              ((limit?.[d] ?? 0) < fleetMax[d] ? "text-destructive" : "text-muted-foreground")
                            }
                          >
                            fleet max {fleetMax[d]}
                          </span>
                        </div>
                      </td>
                    );
                  }
                  return (
                    <td key={c.owner} className="px-4 py-2 text-right tabular-nums">
                      {limit ? limit[d] : <span className="text-muted-foreground">—</span>}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {editing && (
        <div className="flex flex-wrap items-center gap-2 border-t border-border px-4 py-3">
          <Input
            name="note"
            placeholder="Why this change (required)"
            required
            autoFocus
            className="h-8 min-w-64 flex-1 text-sm2"
          />
          <Button type="submit" size="sm" disabled={pending}>
            Save {COLUMNS.find((c) => c.owner === editing)?.label.toLowerCase()} default
          </Button>
          <Button type="button" size="sm" variant="outline" onClick={cancel}>
            Cancel
          </Button>
          {error && (
            <p role="alert" className="basis-full text-sm2 font-medium text-destructive">
              {error}
            </p>
          )}
        </div>
      )}
    </form>
  );
}
