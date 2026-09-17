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
  /** One row ended, because the harness itself stopped it; unknown ids are nothing to record. */
  transitionEnded(session: string, id: string, code?: number | null): ProcRow | undefined {
    const r = this.rows.find((x) => x.session === session && x.id === id && x.ended === undefined);
    if (!r) return undefined;
    r.ended = Date.now();
    if (code !== undefined) r.code = code;
    replaceJson(this.file, this.rows);
    return { ...r };
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


export type PlanItem = { text: string; done?: true };

/**
 * `/bench/plans.json`: what each session said it was going to do. The model writes it with
 * `kl_plan` and ticks items with `kl_plan_done`; the inspector's PLAN panel draws it. Kept on
 * disk so a reconnect or a restart shows the plan rather than an empty panel.
 */
export class Plans {
  private file: string;
  private rows: Record<string, PlanItem[]>;
  constructor(dir: string) {
    this.file = path.join(dir, "plans.json");
    this.rows = readJson<Record<string, PlanItem[]>>(this.file, {});
  }
  set(session: string, items: string[]): PlanItem[] {
    this.rows[session] = items.map((text) => ({ text }));
    replaceJson(this.file, this.rows);
    return this.get(session);
  }
  /** Tick by exact text, else by the first item that contains it — a model rarely quotes itself exactly. */
  done(session: string, item: string): PlanItem[] {
    const list = this.rows[session] ?? [];
    const i = list.findIndex((x) => x.text === item);
    const j = i >= 0 ? i : list.findIndex((x) => !x.done && x.text.toLowerCase().includes(item.toLowerCase()));
    if (j < 0) throw new Error(`no plan item ${JSON.stringify(item)}`);
    list[j].done = true;
    replaceJson(this.file, this.rows);
    return this.get(session);
  }
  get(session: string): PlanItem[] {
    return (this.rows[session] ?? []).map((x) => ({ ...x }));
  }
  all(): { session: string; items: PlanItem[] }[] {
    return Object.entries(this.rows).map(([session, items]) => ({ session, items: items.map((x) => ({ ...x })) }));
  }
  discard(session: string): void {
    if (!(session in this.rows)) return;
    delete this.rows[session];
    replaceJson(this.file, this.rows);
  }
}
