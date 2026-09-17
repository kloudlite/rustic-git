/**
 * Ordering the inbox. While a session is mid-turn, prompts pile up: a person's follow-up, a
 * workspace's reply, an agent's report. FIFO is the wrong order for that pile — the reply that
 * unblocks the work should be taken before a question that can wait — so a FORK of the session
 * (its own context, no tools, answers once) is asked to order it.
 *
 * The parsing lives here, pure: an answer from a model is untrusted input, and the rule when it is
 * not what was asked for is always the same — keep the order that was already there.
 */
export type Ordered = { index: number; reason?: string };

/**
 * `[{index, reason}]` as the fork was asked for, cleaned up: every index exactly once, in range,
 * with anything it forgot appended in its original order. Unparseable answers order nothing.
 */
export function order(answer: string, count: number): Ordered[] | undefined {
  const json = /\[[\s\S]*\]/.exec(answer)?.[0];
  if (!json) return undefined;
  let rows: unknown;
  try {
    rows = JSON.parse(json);
  } catch {
    return undefined;
  }
  if (!Array.isArray(rows)) return undefined;
  const seen = new Set<number>();
  const out: Ordered[] = [];
  for (const r of rows) {
    const i = typeof r === "number" ? r : typeof (r as { index?: unknown })?.index === "number" ? (r as { index: number }).index : undefined;
    if (i === undefined || !Number.isInteger(i) || i < 0 || i >= count || seen.has(i)) continue;
    seen.add(i);
    const reason = typeof (r as { reason?: unknown })?.reason === "string" ? ((r as { reason: string }).reason || undefined) : undefined;
    out.push({ index: i, reason });
  }
  if (!out.length) return undefined;
  // Anything it did not mention keeps its place, at the back: dropping a person's prompt because a
  // model forgot to list it would be losing work, which no ordering is worth.
  for (let i = 0; i < count; i++) if (!seen.has(i)) out.push({ index: i });
  return out;
}

/**
 * An item the harness itself delivered — a workspace reply, an agent report, a finished task, a
 * watch hit — is tagged `[...]` at the front by whatever sent it. Anything else was typed by the
 * PERSON, and their prompts are the ones whose order is not the fork's to change.
 */
export const fromPerson = (text: string): boolean => !/^\s*\[/.test(text);

/**
 * The fork may rank replies, reports and system items freely. It may NOT reorder the person: they
 * typed `build the backend`, saw nothing, typed `retry`, and the fork delivered `retry` first — so
 * pi answered a retry of nothing and then built the backend (owner, 2026-09-17T19-58-02).
 *
 * The person's prompts are pinned back into their original relative order, in the slots the fork
 * chose for them. Everything else keeps the ranking it was given; nothing is added or dropped.
 */
export function keepPersonOrder(rows: Ordered[], items: readonly string[]): Ordered[] {
  const mine = rows.filter((r) => fromPerson(items[r.index] ?? ""));
  if (mine.length < 2) return rows;
  // The slots the person's prompts occupy, and their own order as they were typed.
  const slots = mine.map((_, i) => i);
  const byArrival = [...mine].sort((a, b) => a.index - b.index);
  let at = 0;
  return rows.map((r) => (fromPerson(items[r.index] ?? "") ? byArrival[slots[at++]] : r));
}

/** What the fork is asked. One question, one shape of answer, no conversation. */
export function question(items: string[]): string {
  return [
    "Order this queue most-urgent first. Answer ONLY with JSON: [{index, reason}], reason at most eight words.",
    "Most urgent is what unblocks work already under way, or what the person is waiting on; least urgent is what can be read later.",
    "",
    ...items.map((t, i) => `${i}: ${t.replace(/\s+/g, " ").slice(0, 200)}`),
  ].join("\n");
}
