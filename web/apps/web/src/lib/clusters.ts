/**
 * Pure helpers behind the Clusters area. No fetch, no React — the agent's own
 * `decommission-status` annotation (CLAUDE.md, "Workspaces and environments") is a string, and
 * this is the one place that turns it into something a table can render, so the list and detail
 * pages never parse it their own way.
 */

export type DecommissionParsed =
  | { kind: "draining"; running: number; owned: number; copies: number; thin: number }
  | { kind: "drained"; at: string }
  | { kind: "none" };

const DRAINING = /^draining running=(\d+) owned=(\d+) copies=(\d+) thin=(\d+)$/;
const DRAINED_PREFIX = "drained ";

/** `null`/absent means never drained. An unrecognized non-empty string still counts as
 *  `"draining"` with zero counters rather than `"none"` — a stale or future-shaped stamp should
 *  read as "something is happening" on the row, never as "nothing is happening". */
export function parseDecommissionStatus(raw: string | null | undefined): DecommissionParsed {
  if (!raw) return { kind: "none" };
  if (raw.startsWith(DRAINED_PREFIX)) return { kind: "drained", at: raw.slice(DRAINED_PREFIX.length) };
  const m = DRAINING.exec(raw);
  if (m) return { kind: "draining", running: Number(m[1]), owned: Number(m[2]), copies: Number(m[3]), thin: Number(m[4]) };
  return { kind: "draining", running: 0, owned: 0, copies: 0, thin: 0 };
}

/** Decommission is only ever offered once the node is fully drained. */
export function isDrained(raw: string | null | undefined): boolean {
  return parseDecommissionStatus(raw).kind === "drained";
}

/** `settingsStatus` is an open string on the wire (`"stale"` is a pending backend addition per
 *  the task brief) — anything not `"present"`/`"absent"` reads as neutral rather than erroring. */
export type SettingsStatusTone = "present" | "absent" | "stale" | "unknown";

export function settingsStatusTone(status: string): SettingsStatusTone {
  // The backend spells lag into the value ("stale (lag 2)"), so match the prefix, not the literal.
  if (status.startsWith("stale")) return "stale";
  return status === "present" || status === "absent" ? status : "unknown";
}
