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
 *  rather than a runtime surprise; `usage:{owner}:{dimension}` is built at the call site. */
export const SERIES = [
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
] as const;

/** The sub-line under a KPI's big number. Never "0" for missing history: a flat placeholder that
 *  reads as "nothing changed" is worse than one that says the source is down. `unit` names the
 *  series for the call site's own readability; the sentence itself sits under a tile that is
 *  already labelled with it, so repeating it there would only make the line wrap. */
// `unit` stays in the signature so every tile names its series at the call site, even though the
// sentence itself omits it (the tile above is already labelled with it).
// eslint-disable-next-line @typescript-eslint/no-unused-vars
export function deltaLabel(s: HistorySeries, unit: string): string {
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
  attrs: Record<string, string>;
};

const PHRASES: Record<string, string> = {
  "request.approved": "approved a request for",
  "request.denied": "denied a request for",
  "quota.set": "set the quota for",
  "node.drain": "drained",
  "workload.roll": "rolled",
};

/** One sentence per timeline row. An unrecognised kind falls back to actor + kind + target rather
 *  than being dropped: the events nobody wrote a phrase for are exactly the ones worth seeing. */
export function eventSummary(e: HistoryEvent): string {
  const phrase = PHRASES[e.kind];
  const detail = e.attrs.detail ? ` · ${e.attrs.detail}` : "";
  if (!phrase) return `${e.actor} ${e.kind} ${e.target ?? ""}`.trim();
  const subject = e.owner ?? e.target ?? "";
  return `${e.actor} ${phrase} ${subject}${detail}`.trim();
}

/** `AttentionItem.kind` from `/admin/overview` (and the same words on history rows) → a tone.
 *  Unknown kinds are `warn`, never `neutral`: a row reached the needs-attention feed because
 *  something wanted a person, and greying it out would hide the new thing. */
export function attentionTone(kind: string): Tone {
  if (kind.startsWith("signal.firing") || kind === "critical" || kind === "not_ready") return "critical";
  if (kind === "draining" || kind === "rolling" || kind === "info") return "info";
  return "warn";
}
