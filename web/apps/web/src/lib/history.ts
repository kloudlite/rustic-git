import type { Tone } from "./console";

export type Point = { ts: string; value: number };

/** `GET /admin/history/{series}` (spec §A5) plus one field of ours: `available` is false when the
 *  admin process has no ClickHouse, which is a first-class answer rather than an error — the
 *  console shipped before the history layer did and must keep rendering without it. */
export type HistorySeries = {
  series: Point[];
  summary: { last: number; delta: number; min: number; max: number };
  available: boolean;
};

export const FLAT: HistorySeries = {
  series: [],
  summary: { last: 0, delta: 0, min: 0, max: 0 },
  available: false,
};

/** The fixed series names §A5 lists. A name not in here is a 404 upstream, so it is a typo here
 *  rather than a runtime surprise. */
// eslint-disable-next-line @typescript-eslint/no-unused-vars -- used only as a type below (typeof SERIES)
const SERIES = [
  "pending_requests",
  "firing_signals",
  "owners_over_80",
  "live_workspaces",
  "live_environments",
  "decided_requests",
  "time_to_decide_p50",
  "pool_used",
  "cpu_used",
  "memory_used",
  "restarts",
  "audit_events",
  "usage",
] as const;

export type SeriesName = (typeof SERIES)[number];

/** The sub-line under a KPI's big number. Never "0" for missing history: a flat placeholder that
 *  reads as "nothing changed" is worse than one that says the source is down. The sentence never
 *  names the series — the tile above is already labelled with it. */
export function deltaLabel(s: HistorySeries): string {
  if (!s.available) return "history unavailable";
  if (s.summary.delta === 0) return "unchanged over 7 days";
  const sign = s.summary.delta > 0 ? "+" : "";
  return `${sign}${s.summary.delta} in the last 7 days`;
}

export type HistoryEvent = {
  id: string;
  ts: string;
  kind: string;
  actor: string;
  owner: string | null;
  target: string | null;
  region: string | null;
  attrs: Record<string, string | number | boolean>;
};

/** Kinds actually emitted by `crates/workspaces/src/history/events.rs` and the `admin.*` audit
 *  call sites — a phrase for a kind nothing writes is dead weight, and a kind with no phrase here
 *  falls back to actor + kind + target rather than being dropped. */
const PHRASES: Record<string, string> = {
  "admin.set-quota": "set the quota for",
  "admin.drain": "drained",
  "admin.undrain": "un-drained",
  "admin.decommission": "decommissioned",
  "admin.roll": "rolled",
  "admin.activate-region": "activated",
  "admin.deactivate-region": "deactivated",
  "admin.add-region": "added",
  "admin.stop-workspace": "stopped the workspace for",
  "admin.delete-workspace": "deleted the workspace for",
  "admin.stop-environment": "stopped the environment for",
  "admin.delete-environment": "deleted the environment for",
  "admin.put-central-settings": "updated central settings for",
  "admin.revert-central-settings": "reverted central settings for",
  "request.opened": "opened a request for",
  "request.approved": "approved a request for",
  "request.denied": "denied a request for",
  "workspace.created": "created a workspace for",
  "workspace.started": "started the workspace for",
  "workspace.stopped": "stopped the workspace for",
  "workspace.deleted": "deleted the workspace for",
  "environment.created": "created an environment for",
  "environment.started": "started the environment for",
  "environment.stopped": "stopped the environment for",
  "environment.deleted": "deleted the environment for",
  "volume.released": "released",
  "volume.moved": "moved",
  "volume.unavailable": "made unavailable",
  "volume.deleted": "deleted",
  "node.ready": "reported ready:",
  "node.notready": "reported not ready:",
  "node.cordoned": "cordoned",
  "node.draining": "began draining",
  "node.drained": "finished draining",
  "region.activated": "activated",
  "region.deactivated": "deactivated",
};

/** One sentence per timeline row. An unrecognised kind falls back to actor + kind + target rather
 *  than being dropped: the events nobody wrote a phrase for are exactly the ones worth seeing. */
export function eventSummary(e: HistoryEvent): string {
  const phrase = PHRASES[e.kind];
  if (!phrase) return `${e.actor} ${e.kind} ${e.target ?? ""}`.trim();
  const subject = e.owner ?? e.target ?? "";
  return `${e.actor} ${phrase} ${subject}`.trim();
}

/** `AttentionItem.kind` from `/admin/overview` (and the same words on history rows) → a tone.
 *  Unknown kinds are `warn`, never `neutral`: a row reached the needs-attention feed because
 *  something wanted a person, and greying it out would hide the new thing. */
export function attentionTone(kind: string): Tone {
  if (kind.startsWith("signal.firing") || kind === "critical" || kind === "not_ready") return "critical";
  if (kind === "draining" || kind === "rolling" || kind === "info") return "info";
  return "warn";
}
