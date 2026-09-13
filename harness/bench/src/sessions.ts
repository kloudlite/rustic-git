import path from "node:path";
import { readJson, replaceJson } from "./log.ts";

export type SessionRow = { id: string; name: string; seq: number; file?: string; created: number; lastActive: number; archived: boolean; model?: string };

type Stored = SessionRow[] | { nextSeq: number; rows: SessionRow[] };

/**
 * `/bench/sessions.json`: the list is small, so every change replaces it whole.
 * The high-water mark `nextSeq` is stored beside the rows so a removed id is
 * never reused (session files, trash and exchange rows are keyed by it) —
 * an old bare-array file is read as rows with nextSeq = max(seq)+1.
 */
export class SessionList {
  private file: string;
  private rows: SessionRow[];
  private nextSeq: number;
  constructor(dir: string) {
    this.file = path.join(dir, "sessions.json");
    const stored = readJson<Stored>(this.file, []);
    if (Array.isArray(stored)) {
      this.rows = stored;
      this.nextSeq = Math.max(0, ...stored.map((r) => r.seq)) + 1;
    } else {
      this.rows = stored.rows;
      this.nextSeq = stored.nextSeq;
    }
  }
  private save() {
    replaceJson(this.file, { nextSeq: this.nextSeq, rows: this.rows });
  }
  all(): SessionRow[] {
    return this.rows.map((r) => ({ ...r }));
  }
  get(id: string): SessionRow | undefined {
    return this.rows.find((r) => r.id === id);
  }
  create(model?: string): SessionRow {
    const seq = Math.max(this.nextSeq, Math.max(0, ...this.rows.map((r) => r.seq)) + 1);
    this.nextSeq = seq + 1;
    const now = Date.now();
    const row: SessionRow = { id: `s-${seq}`, name: `session ${seq}`, seq, created: now, lastActive: now, archived: false, model };
    this.rows.push(row);
    this.save();
    return { ...row };
  }
  update(id: string, patch: Partial<SessionRow>): SessionRow {
    const r = this.get(id);
    if (!r) throw new Error(`no session ${id}`);
    Object.assign(r, patch, { id: r.id });
    this.save();
    return { ...r };
  }
  remove(id: string): void {
    this.rows = this.rows.filter((r) => r.id !== id);
    this.save();
  }
  merge(rows: SessionRow[]): string[] {
    const added = rows.filter((r) => !this.get(r.id));
    if (!added.length) return [];
    this.rows.push(...added.map((r) => ({ ...r })));
    this.nextSeq = Math.max(this.nextSeq, ...added.map((r) => r.seq + 1));
    this.save();
    return added.map((r) => r.id);
  }
}
