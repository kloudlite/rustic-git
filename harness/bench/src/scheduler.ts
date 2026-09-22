// The one loop (spec §2): on boot, on every inbound write and when a turn ends, start a turn for
// every open session with unread rows, oldest seq first, never more than MAX_RUNNING at once.
// Boot also repairs the two two-write orders from spec §1.
import { append, openTurn, readRows, lastEnd, type Row } from "./rows.ts";
import type { Session } from "./session.ts";
import type { SessionList, SessionRow } from "./sessions.ts";

export const MAX_RUNNING = 8; // ponytail: one fixed cap; per-tier caps if top sessions starve

export class Scheduler {
  sessions = new Map<number, Session>();
  private list: SessionList;
  private make: (row: SessionRow) => Session;
  private max: number;
  constructor(list: SessionList, make: (row: SessionRow) => Session, max = MAX_RUNNING) {
    this.list = list;
    this.make = make;
    this.max = max;
  }

  get(seq: number) { return this.sessions.get(seq); }

  private actor(row: SessionRow): Session {
    let s = this.sessions.get(row.seq);
    if (!s) { s = this.make(row); s.onAnswer = async (c, end) => this.deliver(c, end); this.sessions.set(row.seq, s); }
    return s;
  }

  boot() {
    for (const row of this.list.all()) {
      const file = this.list.logFile(row);
      const rows = readRows(file);
      const open = openTurn(rows);
      if (open !== undefined) {
        append(file, { kind: "interrupted", ts: Date.now(), turn: open });
        // The parent decides what happens to an interrupted child (spec §5): told once, never resumed.
        const p = row.parent !== undefined ? this.list.bySeq(row.parent) : undefined;
        if (p) append(this.list.logFile(p), { kind: "user", ts: Date.now(), from: row.seq, text: `child ${row.seq} was interrupted by a bench restart; delegate to it again to resume on its clone, or leave it` });
      }
      // A child whose newest answer never reached its parent: the crash fell between the two writes.
      if (row.parent !== undefined) {
        const end = lastEnd(rows);
        const parent = this.list.bySeq(row.parent);
        if (end?.answer !== undefined && parent && !readRows(this.list.logFile(parent)).some((r) => r.kind === "user" && r.from === row.seq && r.childTurn === end.turn)) {
          append(this.list.logFile(parent), { kind: "user", ts: Date.now(), from: row.seq, text: end.answer, childTurn: end.turn });
        }
      }
    }
    this.kick();
  }

  kick() {
    const running = [...this.sessions.values()].filter((s) => s.running).length;
    let slots = this.max - running;
    for (const row of this.list.all().sort((a, b) => a.seq - b.seq)) {
      if (slots <= 0) break;
      if (row.state === "closed" || row.archived) continue;
      const s = this.actor(row);
      if (s.running || !s.hasUnread()) continue;
      slots--;
      void s.turn().finally(() => this.kick());
    }
  }

  deliver(child: Session, end: Extract<Row, { kind: "turn.end" }>) {
    const parent = child.row.parent !== undefined ? this.list.bySeq(child.row.parent) : undefined;
    if (parent && end.answer !== undefined) this.actor(parent).receive(child.row.seq, end.answer, end.turn);
    this.kick();
  }
}
