import { SessionManager } from "@earendil-works/pi-coding-agent";

/**
 * History without an agent: pi's SDK reads its own JSONL (tree, compaction,
 * branch summaries resolved) and hands back the messages the model would see.
 * This is all `--read-only` needs, and what serves an archived session.
 */
export function transcript(file: string): unknown[] {
  return SessionManager.open(file).buildSessionContext().messages;
}

export function page<T>(all: T[], after = 0, limit = all.length, tail = 0): { messages: T[]; total: number; from: number } {
  // `tail` is what a window opening on a long thread asks for: the newest N, and where they start,
  // so it can ask for the ones before them later. Opening a thread used to fetch the whole history
  // through pi before anything was drawn (owner, 2026-09-17).
  const from = tail ? Math.max(0, all.length - tail) : after;
  return { messages: all.slice(from, tail ? undefined : from + limit), total: all.length, from };
}
