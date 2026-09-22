// One append-only jsonl per session is the whole conversation (spec §1). Everything the scheduler
// needs — what is unread, whether a turn is open — is derived from the rows on every read, never
// stored beside them, so a crash between two writes can only lose work, never invent state.
import { appendLine, readLines } from "./log.ts";

export type Row =
  | { kind: "user"; ts: number; from: "person" | number; text: string; childTurn?: number }
  | { kind: "turn.start"; ts: number; turn: number }
  | { kind: "turn.step"; ts: number; turn: number; step: string }
  | { kind: "turn.end"; ts: number; turn: number; answer?: string; error?: string; commit?: string }
  | { kind: "delegate"; ts: number; turn: number; child: number; instruction: string }
  | { kind: "interrupted"; ts: number; turn: number };

export const readRows = (file: string): Row[] => readLines<Row>(file);
export const append = (file: string, row: Row) => appendLine(file, row);

export function unread(rows: Row[]) {
  let from = 0;
  rows.forEach((r, i) => { if (r.kind === "turn.end") from = i + 1; });
  return rows.slice(from).filter((r): r is Extract<Row, { kind: "user" }> => r.kind === "user");
}

export function openTurn(rows: Row[]): number | undefined {
  let open: number | undefined;
  for (const r of rows) {
    if (r.kind === "turn.start") open = r.turn;
    else if ((r.kind === "turn.end" || r.kind === "interrupted") && r.turn === open) open = undefined;
  }
  return open;
}

export const nextTurn = (rows: Row[]) => 1 + rows.reduce((m, r) => ("turn" in r ? Math.max(m, r.turn) : m), 0);

export function lastEnd(rows: Row[]) {
  for (let i = rows.length - 1; i >= 0; i--) if (rows[i].kind === "turn.end") return rows[i] as Extract<Row, { kind: "turn.end" }>;
  return undefined;
}
