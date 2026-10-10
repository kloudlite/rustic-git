import type { Entry } from "./components/Transcript.tsx";

type Tool = Entry & { kind: "tool" };
const isTool = (e: Entry): e is Tool => e.kind === "tool";

/**
 * A retry replaces the failed row instead of stacking under it: when `next` is a tool entry and the
 * latest tool entry before it is the same tool, failed, with no user message since, drop that row
 * and carry a retry count forward. Anything else appends.
 */
export function foldRetry(entries: Entry[], next: Entry): Entry[] {
  if (!isTool(next)) return [...entries, next];
  for (let i = entries.length - 1; i >= 0; i--) {
    const e = entries[i]!;
    if (e.kind === "user") break;
    if (!isTool(e)) continue;
    if (e.name !== next.name || e.status !== "error") break;
    return [...entries.slice(0, i), ...entries.slice(i + 1), { ...next, retries: (e.retries ?? 0) + 1 }];
  }
  return [...entries, next];
}

/** Reload: tool status arrives with the later toolResult, so fold retries over the finished list. */
export const foldRetries = (entries: Entry[]): Entry[] => entries.reduce<Entry[]>(foldRetry, []);

/**
 * Codemode inner calls arrive as their own tool entries (ids `<parent>/<n>`). A run of consecutive
 * inner calls with the same name and summary collapses into the latest, counted in `repeats`.
 */
export function foldRepeats(entries: Entry[]): Entry[] {
  const out: Entry[] = [];
  for (const e of entries) {
    const p = out[out.length - 1];
    if (p && isTool(e) && isTool(p) && e.id?.includes("/") && p.id?.includes("/") && p.name === e.name && p.summary === e.summary) {
      out[out.length - 1] = { ...e, id: p.id, repeats: (p.repeats ?? 1) + 1 };
    } else out.push(e);
  }
  return out;
}
