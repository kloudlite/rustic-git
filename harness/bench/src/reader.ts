import { SessionManager } from "@mariozechner/pi-coding-agent";

/**
 * History without an agent: pi's SDK reads its own JSONL (tree, compaction,
 * branch summaries resolved) and hands back the messages the model would see.
 * This is all `--read-only` needs, and what serves an archived session.
 */
export function transcript(file: string): unknown[] {
  return SessionManager.open(file).buildSessionContext().messages;
}

export function page<T>(all: T[], after = 0, limit = all.length): { messages: T[]; total: number } {
  return { messages: all.slice(after, after + limit), total: all.length };
}
