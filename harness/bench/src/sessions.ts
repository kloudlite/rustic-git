import path from "node:path";
import { readJson, replaceJson } from "./log.ts";

export type SessionRow = { id: string; name: string; seq: number; file?: string; created: number; lastActive: number; archived: boolean; model?: string };

/** `/bench/sessions.json`: the list is small, so every change replaces it whole. */
export class SessionList {
  private file: string;
  private rows: SessionRow[];
  constructor(dir: string) {
    this.file = path.join(dir, "sessions.json");
    this.rows = readJson<SessionRow[]>(this.file, []);
  }
  private save() {
    replaceJson(this.file, this.rows);
  }
  all(): SessionRow[] {
    return this.rows.map((r) => ({ ...r }));
  }
  get(id: string): SessionRow | undefined {
    return this.rows.find((r) => r.id === id);
  }
  create(model?: string): SessionRow {
    const seq = Math.max(0, ...this.rows.map((r) => r.seq)) + 1;
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
    this.save();
    return added.map((r) => r.id);
  }
}
