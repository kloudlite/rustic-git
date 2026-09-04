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
