/** The six dimensions a quota has, in the order the bar shows them. The words are the api's own
 *  field names, so a 409 naming one is directly a key here — one vocabulary, not two. */
export const DIMS = ["workspaces", "environments", "snapshots", "diskGb", "cpu", "memoryGb"] as const;
export type QuotaDim = (typeof DIMS)[number];

export type QuotaReport = {
  owner: string;
  limit: Record<QuotaDim, number>;
  used: Record<QuotaDim, number>;
};

export function dimLabel(d: QuotaDim): string {
  return {
    workspaces: "Workspaces",
    environments: "Environments",
    snapshots: "Snapshots",
    diskGb: "Disk",
    cpu: "CPU",
    memoryGb: "Memory",
  }[d];
}

/** A whole percentage for the bar's width. A zero limit reads FULL rather than NaN — it is a
 *  dimension nobody may use — and over-quota clamps, because /v1 is read-then-write and a limit
 *  can be lowered under existing use. */
export function percent(used: number, limit: number): number {
  if (limit <= 0) return 100;
  return Math.min(100, Math.round((used / limit) * 100));
}

export function atLimit(r: QuotaReport, d: QuotaDim): boolean {
  return r.used[d] >= r.limit[d];
}

/** The dimension a 409 named, so the request form opens on the field that blocked them.
 *  The sentence is fixed by the api (`quota::refuse`); anything else is not a quota refusal. */
export function dimFromRefusal(message: string): QuotaDim | null {
  const word = message.split(":")[0]?.trim();
  return (DIMS as readonly string[]).includes(word) && message.includes("request more under Quota")
    ? (word as QuotaDim)
    : null;
}
