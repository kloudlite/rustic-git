/**
 * Pure helpers behind the Workloads/roll UI (Monitoring and Clusters tabs). No fetch, no React —
 * everything here is a function of what the roll table already has, so it is testable with
 * `bun:test` and both tabs share it instead of re-deriving "is this settled" or "what does a
 * conflict mean" their own way.
 */

/** A `409` save conflict's body (`workloads::conflict` on the api tier: `{name, ready, desired}`)
 *  turned into the plain-English sentence — "no retry loop, the operator retries by hand", so
 *  this is display text, not a signal the caller loops on. Falls back to the raw text when the
 *  body isn't the shape expected (defensive, not expected in practice). */
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

/** The Workloads row's one derived label — `WorkloadDoc.rolloutState` is already `"RollingOut"` /
 *  `"Stable"` from the server, this only adds the ready/desired count the row shows next to it. */
export function rolloutStateLabel(rolloutState: "RollingOut" | "Stable", ready: number, desired: number): string {
  return rolloutState === "Stable" ? "Stable" : `Rolling out (${ready}/${desired} ready)`;
}

/** A workload has settled once it reports ready == desired — what the roll table's own
 *  poll-until-settled loop uses to decide whether to keep auto-refreshing. */
export function settled(w: { ready: number; desired: number }): boolean {
  return w.ready >= w.desired;
}

/** A schema row's value, for display: `null`/`undefined` reads as "not set" rather than "null"
 *  or an empty cell, and a bool prints its word rather than JS's `Boolean.toString` coincidence. */
export function fmt(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (typeof v === "boolean") return v ? "true" : "false";
  return String(v);
}

/** What the editor holds per field: a boolean for a `bool` row, the typed text for the rest. */
export type Draft = Record<string, string | boolean>;

type EditRow = { name: string; unit: string; env: string | null; default: unknown };

/** The control's starting value. A bool starts at the value in force (a checkbox has no "unset");
 *  anything else starts at the STORED value or empty, since an empty box means "leave it". */
export function initialDraft(row: EditRow, stored: unknown): string | boolean {
  if (row.unit === "bool") {
    const v = effectiveValue(stored, row.env, row.default).value;
    return v === true || v === "true";
  }
  return stored === null || stored === undefined ? "" : String(stored);
}

/** Only the fields the person actually moved. The PUT merges (a field it is not sent keeps its
 *  stored value), so sending an untouched one would re-stamp it as stored and hide its default.
 *  Unparsable number text is sent as-is so the api's own 422 names the field, never swallowed. */
export function changedFields(rows: EditRow[], stored: Record<string, unknown>, draft: Draft): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const row of rows) {
    const d = draft[row.name];
    if (d === undefined || d === initialDraft(row, stored[row.name])) continue;
    if (typeof d === "boolean") out[row.name] = d;
    else if (d.trim() === "") continue;
    else if (row.unit === "string") out[row.name] = d;
    else out[row.name] = Number.isFinite(Number(d)) ? Number(d) : d;
  }
  return out;
}

/** The flat body both PUT routes take: the fields beside a required `note`. */
export function saveBody(changes: Record<string, unknown>, note: string): Record<string, unknown> {
  return { ...changes, note: note.trim() };
}

export function canSave(changes: Record<string, unknown>, note: string): boolean {
  return Object.keys(changes).length > 0 && note.trim() !== "";
}

/** A failed write's sentence: a 422 is shown verbatim (it names the field), a 409 in words. */
export function writeError(r: { kind: string; message: string }): string {
  return r.kind === "conflict" ? conflictMessage(r.message) : r.message;
}

/** Central keeps history inline; a region keeps it in the CR's annotation as a JSON string. */
export function historyOf(doc: { history?: unknown; metadata?: { annotations?: Record<string, string> } }): Record<string, unknown>[] {
  if (Array.isArray(doc.history)) return doc.history as Record<string, unknown>[];
  try {
    return JSON.parse(doc.metadata?.annotations?.["kloudlite.io/settings-history"] ?? "[]");
  } catch {
    return [];
  }
}

/** The Configuration page's whole point: `stored ?? env ?? default`, restated in the web tier
 *  the same order the reader itself resolves a knob (per `CLAUDE.md`'s "Live settings"), so the
 *  page can label which of the three actually won without asking the backend to say so. */
export function effectiveValue(
  stored: unknown,
  env: string | null,
  builtinDefault: unknown,
): { value: unknown; source: "stored" | "env" | "default" } {
  if (stored !== null && stored !== undefined) return { value: stored, source: "stored" };
  if (env !== null && env !== undefined) return { value: env, source: "env" };
  return { value: builtinDefault, source: "default" };
}
