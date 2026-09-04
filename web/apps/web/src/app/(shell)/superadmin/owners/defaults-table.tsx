"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { QuotaReport } from "@/lib/quota";
import { DIMS, dimLabel, dimUnit, type QuotaDim } from "@/lib/quota";
import { Section } from "../ui/section";
import { DataTable, Td, Th, Tr } from "../ui/data-table";
import { writeDefault } from "../actions";

type DefaultOwner = "default-user" | "default-team";

const COLUMNS: { owner: DefaultOwner; hint: string }[] = [
  { owner: "default-user", hint: "an owner with no quota of their own" },
  { owner: "default-team", hint: "a team with no quota of its own" },
];

/** The two defaults as ONE comparison table — dimensions down, person and team across — so the
 *  columns read against each other, which is the only question this section answers ("is the team
 *  default really 4× the person one?"). Editing turns one column into inputs in place; the other
 *  column stays readable so the operator keeps the comparison while typing. */
export function DefaultsTable({
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

  return (
    <form action={save}>
      <Section eyebrow="Policy" title="Defaults for an owner with no quota of their own" bare>
        <DataTable>
          <thead>
            <tr>
              <Th>Dimension</Th>
              <Th>Unit</Th>
              {COLUMNS.map((c) => (
                <Th key={c.owner} numeric>
                  <span className="inline-flex items-center gap-2">
                    <span title={c.hint}>{c.owner}</span>
                    {editing !== c.owner && (
                      <Button
                        type="button"
                        size="sm"
                        variant="outline"
                        className="h-5 px-1.5 text-micro normal-case tracking-normal"
                        disabled={editing !== null || !limits[c.owner]}
                        onClick={() => setEditing(c.owner)}
                      >
                        Edit
                      </Button>
                    )}
                  </span>
                </Th>
              ))}
            </tr>
          </thead>
          <tbody>
            {DIMS.map((d) => (
              <Tr key={d}>
                <Td>{dimLabel(d)}</Td>
                <Td className="text-muted-foreground">{dimUnit(d) || "count"}</Td>
                {COLUMNS.map((c) => {
                  const limit = limits[c.owner];
                  if (editing === c.owner) {
                    return (
                      <Td key={c.owner} numeric>
                        <div className="ml-auto flex w-40 flex-col items-end gap-0.5 py-1">
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
                      </Td>
                    );
                  }
                  return (
                    <Td key={c.owner} numeric>
                      {limit ? limit[d] : <span className="text-muted-foreground">—</span>}
                    </Td>
                  );
                })}
              </Tr>
            ))}
          </tbody>
        </DataTable>
        {editing ? (
          <div className="flex flex-wrap items-center gap-2 border-t border-border px-4 py-2">
            <Input
              name="note"
              placeholder="Why this change (required)"
              required
              autoFocus
              className="h-8 min-w-64 flex-1 text-sm2"
            />
            <Button type="submit" size="sm" disabled={pending}>
              Save {editing}
            </Button>
            <Button type="button" size="sm" variant="outline" onClick={() => { setEditing(null); setError(null); }}>
              Cancel
            </Button>
            {error && (
              <p role="alert" className="basis-full text-sm2 font-medium text-destructive">
                {error}
              </p>
            )}
          </div>
        ) : (
          <p className="border-t border-border px-4 py-2 text-caption text-muted-foreground">
            An owner with no Quota object of their own falls back to these; a compiled-in table backs
            them, so nothing is ever unlimited.
          </p>
        )}
      </Section>
    </form>
  );
}
