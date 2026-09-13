import path from "node:path";
import { appendLine, readJson, readLines, replaceJson } from "./log.ts";

export type TaskRow = { id: string; session: string; n?: number; tool: string; arg: string; state: "running" | "background" | "done" | "failed" | "cancelled" | "lost"; started: number; ended?: number };

/** `/bench/tasks.jsonl`: one line per transition; the current table is the fold. */
export class Tasks {
  private file: string;
  private rows = new Map<string, TaskRow>();
  constructor(dir: string) {
    this.file = path.join(dir, "tasks.jsonl");
    for (const l of readLines<Partial<TaskRow> & { id: string }>(this.file)) this.fold(l);
  }
  private fold(l: Partial<TaskRow> & { id: string }): TaskRow {
    const row = { ...(this.rows.get(l.id) ?? {}), ...l } as TaskRow;
    this.rows.set(l.id, row);
    return row;
  }
  transition(t: Partial<TaskRow> & { id: string }): TaskRow {
    appendLine(this.file, t);
    return this.fold(t);
  }
  all(): TaskRow[] {
    return [...this.rows.values()];
  }
  /** A new process holds none of the old one's commands: whatever was in flight is gone. */
  markLost(session?: string): TaskRow[] {
    const now = Date.now();
    return this.all().filter((t) => (t.state === "running" || t.state === "background") && (session === undefined || t.session === session)).map((t) => this.transition({ id: t.id, state: "lost", ended: now }));
  }
}

export type ProcRow = { id: string; session: string; name: string; command: string; pid?: number; started: number; ended?: number; code?: number | null; lost?: true };

/** `/bench/procs.json`: the live table, replaced whole; each session publishes only its own rows. */
export class Procs {
  private file: string;
  private rows: ProcRow[];
  constructor(dir: string) {
    this.file = path.join(dir, "procs.json");
    this.rows = readJson<ProcRow[]>(this.file, []);
  }
  snapshot(session: string, rows: Omit<ProcRow, "session">[]): void {
    this.rows = [...this.rows.filter((r) => r.session !== session), ...rows.map((r) => ({ ...r, session }))];
    replaceJson(this.file, this.rows);
  }
  all(): ProcRow[] {
    return this.rows.map((r) => ({ ...r }));
  }
  /**
   * Every open row, whatever its pid: pids restart low in a new container, so an
   * old row's pid can name a live, unrelated process and hold the bench awake forever.
   */
  markLost(session?: string): ProcRow[] {
    const now = Date.now();
    const lost = this.rows.filter((r) => r.ended === undefined && (session === undefined || r.session === session));
    for (const r of lost) Object.assign(r, { ended: now, lost: true as const });
    if (lost.length) replaceJson(this.file, this.rows);
    return lost.map((r) => ({ ...r }));
  }
}
