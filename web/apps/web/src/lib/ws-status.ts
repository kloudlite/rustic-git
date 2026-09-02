/** One place the states a workspace or environment can be *waiting on* become sentences.
 *
 *  Pure and in `lib` so it is testable without rendering: the list components render `noticesFor`
 *  and nothing decides these words twice. The API answers with the conditions the node wrote; this
 *  file is the only translation of them into English, and the messages deliberately say what the
 *  person can DO rather than restating the condition's reason. */
export type WsNotice = { tone: "info" | "warning"; text: string };

type ConditionDoc = { ready: boolean; reason: string; message: string };

export function noticesFor(x: {
  state: string;
  replicated?: ConditionDoc | null;
  degraded?: ConditionDoc | null;
  decommissioning?: ConditionDoc | null;
}): WsNotice[] {
  // Interrupted first: it is the only one that changes what the buttons can do (start is refused,
  // clone is the way forward), so it must not be buried under a copying notice.
  if (x.degraded?.ready && x.degraded.reason === "NodeDead") {
    return [{ tone: "warning", text: "Its node is down. It resumes when the node returns — or clone it from the last synced point." }];
  }
  if (x.decommissioning?.ready && x.decommissioning.reason === "NodeLeaving") {
    return [{ tone: "info", text: "This node is being retired; stop when convenient and the next start lands elsewhere." }];
  }
  const r = x.replicated;
  if (x.state === "stopped" && r) {
    if (r.ready) return [{ tone: "info", text: "Copied to another node — safe to start anywhere." }];
    // The `replicas: 1` case shares a reason with "not yet" on purpose (one condition, one place
    // to read it) and is told apart by the message the node wrote.
    if (r.message.startsWith("no replica is configured")) {
      return [{ tone: "info", text: "No replica is configured, so this can only ever start on its current node." }];
    }
    return [{ tone: "info", text: "Still copying to another node — it can only start on its current node until that finishes." }];
  }
  return [];
}

/** What a clone was grafted onto. Always shown: a clone is always based on a cut, and the
 *  interrupted case differs only in that the cut is older than "now" — which is precisely the
 *  thing the person accepted when they chose it. The age comes from the API's own `age_seconds`,
 *  never from this browser's clock: only the node knows when the source stopped moving. */
export function basedOnSentence(b: { snapshot: string; at?: string | null; age_seconds: number; interrupted: boolean }): string {
  if (!b.at) return "Cloned from a sync point taken just now.";
  const time = new Date(b.at).toISOString().slice(11, 19);
  if (!b.interrupted) return `Cloned from the sync point of ${time}.`;
  const mins = Math.max(0, Math.round(b.age_seconds / 60));
  const ago = mins === 1 ? "1 minute" : `${mins} minutes`;
  return `Cloned from the sync point of ${time}, ${ago} before the node went down.`;
}

/** The clone response's `based_on` as the one thing the dialog says back. Pure and here so the
 *  server action is a call rather than a branch nothing can test without a request. */
export function cloneResult(v: { based_on?: { snapshot: string; at?: string | null; age_seconds: number; interrupted: boolean } | null }): {
  ok: true;
  basedOn?: string;
} {
  return { ok: true, basedOn: v.based_on ? basedOnSentence(v.based_on) : undefined };
}
