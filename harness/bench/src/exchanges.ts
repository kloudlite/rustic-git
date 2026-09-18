import path from "node:path";
import { appendLine, readLines } from "./log.ts";
import { isActiveExchangeState, type ExchangeState } from "./exchange-state.ts";

export type Exchange = { ts: number; id: string; session: string; workspace: string; dir: "in" | "out"; text: string; state: ExchangeState; ref?: string };
type Line = Partial<Exchange> & { ts: number; discarded?: true };

/**
 * `/bench/exchanges.jsonl`: every message between a session and a workspace,
 * one log. A session's queue and a workspace's queue are two filters over the
 * same rows, so they cannot disagree. Discard (a deleted session) is an
 * appended `discarded` row, never an edit of earlier lines — the log stays
 * append-only and a restart folds the same result.
 */
export class ExchangeLog {
  private file: string;
  private rows = new Map<string, Exchange>();
  constructor(dir: string) {
    this.file = path.join(dir, "exchanges.jsonl");
    for (const l of readLines<Line>(this.file)) this.fold(l);
  }
  private fold(l: Line) {
    if (l.discarded && l.session) {
      for (const [id, e] of this.rows) if (e.session === l.session) this.rows.delete(id);
    } else if (l.session && l.workspace && l.id) {
      this.rows.set(l.id, l as Exchange);
    } else if (l.id && l.state) {
      const e = this.rows.get(l.id);
      if (e) this.rows.set(l.id, { ...e, state: l.state });
    }
  }
  private write(l: Line) {
    appendLine(this.file, l);
    this.fold(l);
  }
  record(e: Omit<Exchange, "ts">): Exchange {
    const row = { ts: Date.now(), ...e };
    this.write(row);
    return row;
  }
  transition(id: string, state: string): void {
    this.write({ ts: Date.now(), id, state: state as ExchangeState });
  }
  discard(session: string): void {
    this.write({ ts: Date.now(), session, discarded: true });
  }
  private where(pred: (e: Exchange) => boolean, after = 0): Exchange[] {
    return [...this.rows.values()].filter((e) => pred(e) && e.ts > after).sort((a, b) => a.ts - b.ts);
  }
  bySession(session: string, after?: number): Exchange[] {
    return this.where((e) => e.session === session, after);
  }
  byWorkspace(workspace: string, after?: number): Exchange[] {
    return this.where((e) => e.workspace === workspace, after);
  }
  get(id: string): Exchange | undefined {
    return this.rows.get(id);
  }
  active(): Exchange[] {
    return this.where((e) => e.dir === "out" && isActiveExchangeState(e.state));
  }
  recent(n: number): Exchange[] {
    return this.where(() => true).slice(-n);
  }
}
