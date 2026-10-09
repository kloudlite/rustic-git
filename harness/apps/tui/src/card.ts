//! A message another session sent arrives as a user turn: `[from {key}] {kind}: {body}` (kind optional,
//! older ones end in a `board: …` line the card drops). The transcript draws it as a card, not a bubble.
export type Card = { from: string; kind?: "done" | "blocked" | "question" | "failed"; body: string };

export function parseCard(text: string): Card | null {
  const m = /^\[from ([^\]]+)\] ?(?:(done|blocked|question|failed): )?([\s\S]*)$/.exec(text);
  if (!m) return null;
  const from = m[1] === "main session" ? "main" : m[1]!;
  return { from, kind: m[2] as Card["kind"], body: m[3]!.replace(/\n+board:[^\n]*\s*$/, "").trim() };
}
