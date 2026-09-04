/**
 * Pure helpers behind the `/admin/settings` tabs (spec §5/§7). No fetch, no React — everything
 * here is a function of the rows the schema+value fetch already produced, so it is testable with
 * `bun:test` and the two settings tabs (central, one per cluster region) share it instead of each
 * re-deriving "what changed" or "what does Save mean" its own way.
 */

export type SettingRow = {
  key: string;
  description: string;
  unit: string;
  /** Current value: `stored ?? env ?? default`, already resolved server-side. */
  value: unknown;
  envValue: unknown;
  defaultValue: unknown;
  range: { min: number; max: number } | null;
  mark: "live" | "boot";
  /** Workload names a boot-marked field's change rolls. Empty for a live field. */
  readers: string[];
  /** Document-level, not per-field: neither `StoredCentralSettings` nor the `ClusterSettings`
   *  CR's annotations track who touched which individual key, only who last wrote the document. */
  lastChangedBy?: string;
  lastChangedAt?: string;
};

/** Structurally what `api.SettingsSchemaRow` is — restated here rather than imported, since this
 *  file has to stay importable from `bun:test` (`api.ts` is `import "server-only"`, which throws
 *  outside a server-component render) and the two shapes cannot drift: `getSettingsSchema`'s
 *  return type IS this shape. */
type SchemaRow = {
  name: string;
  description: string;
  unit: string;
  range: { min: number; max: number } | null;
  mark: "live" | "boot";
  readers: string[];
  default: unknown;
  env: string | null;
};

/** One scope's schema rows plus its current values, turned into what `SettingsTable` renders —
 *  the merge the brief asks the admin GET routes to save the web from hand-duplicating. */
export function mergeRows(
  schemaRows: SchemaRow[],
  values: Record<string, unknown>,
  lastChangedBy?: string,
  lastChangedAt?: string,
): SettingRow[] {
  return schemaRows.map((r) => ({
    key: r.name,
    description: r.description,
    unit: r.unit,
    value: values[r.name] ?? r.default,
    envValue: r.env,
    defaultValue: r.default,
    range: r.range,
    mark: r.mark,
    readers: r.readers,
    lastChangedBy,
    lastChangedAt,
  }));
}

/** Only the rows whose edited value actually differs from the fetched one — the diff Save posts,
 *  so an untouched field is never re-sent (and never rolls a reader it didn't change). */
export function changedFields(rows: SettingRow[], edited: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const row of rows) {
    if (!(row.key in edited)) continue;
    if (edited[row.key] !== row.value) out[row.key] = edited[row.key];
  }
  return out;
}

/** The deduped, sorted union of readers across every changed row that is `mark: "boot"` — spec
 *  §7's "Save and roll: {reader list, deduped across every changed boot field}". */
export function bootReaders(rows: SettingRow[], changedKeys: string[]): string[] {
  const set = new Set<string>();
  for (const key of changedKeys) {
    const row = rows.find((r) => r.key === key);
    if (row?.mark === "boot") for (const reader of row.readers) set.add(reader);
  }
  return [...set].sort();
}

export type Confirmation =
  | { needsConfirm: false }
  | { needsConfirm: true; message: string; readers: string[]; needsSecondConfirm: boolean };

/** What the confirmation dialog says, decided BEFORE anything is posted — spec §7's exact wording:
 *  a live-only diff saves with no dialog, a boot-marked diff names the roll, and `rustic-git-srv`
 *  among the readers (only reachable from a central write — no cluster boot field's reader is
 *  ever the StatefulSet) earns a second confirmation naming the DB-ownership-move risk. */
export function confirmationFor(rows: SettingRow[], changedKeys: string[]): Confirmation {
  const readers = bootReaders(rows, changedKeys);
  if (readers.length === 0) return { needsConfirm: false };
  return {
    needsConfirm: true,
    message: `Save and roll: ${readers.join(", ")}`,
    readers,
    needsSecondConfirm: readers.includes("rustic-git-srv"),
  };
}

/** Which of a just-saved diff's keys are still "pending" — the saved value has not yet shown up
 *  in the latest poll of the same scope's current values (`observedGeneration` for clusters, the
 *  `/healthz` version for central are what make that poll's `current` fresh; this function only
 *  compares values, the caller decides when to re-poll). */
export function pendingKeys(saved: Record<string, unknown>, current: Record<string, unknown>): string[] {
  return Object.keys(saved).filter((k) => saved[k] !== current[k]);
}

/** A `409` save conflict's body (`workloads::conflict` on the api tier: `{name, ready, desired}`)
 *  turned into the plain-English sentence spec §7 asks for — "no retry loop, the operator retries
 *  by hand", so this is display text, not a signal the caller loops on. Falls back to the raw
 *  text when the body isn't the shape expected (defensive, not expected in practice). */
export function conflictMessage(raw: string): string {
  try {
    const body = JSON.parse(raw) as { name?: string; ready?: number; desired?: number };
    if (body.name && body.ready !== undefined && body.desired !== undefined) {
      return `${body.name} is still rolling out (${body.ready}/${body.desired} ready); try again shortly`;
    }
  } catch {
    // Not the JSON envelope — the raw text is still better than nothing.
  }
  return raw;
}
