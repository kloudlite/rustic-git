import fs from "node:fs";
import path from "node:path";
import { readJson, replaceJson } from "./log.ts";

export type SessionKind = "bench" | "workspace" | "ephemeral";
/**
 * `kind` absent is a bench session. `target` is the workspace whose tool server runs a thread's tools.
 * `parent`/`tier`/`state` describe the sys-1 session tree: `parent` is the owning row's seq, `tier`
 * places a row as the top bench session, a main per-workspace session, or a sub-session spawned under one.
 */
export type SessionRow = { id: string; name: string; seq: number; file?: string; created: number; lastActive: number; archived: boolean; model?: string; kind?: SessionKind; workspace?: string; target?: string; parent?: number; tier?: "top" | "main" | "sub"; state?: "open" | "closed" };

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
  private dir: string;
  constructor(dir: string) {
    this.dir = dir;
    this.file = path.join(dir, "sessions.json");
    fs.mkdirSync(path.join(dir, "sessions"), { recursive: true });
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
  create(model?: string, extra: Partial<SessionRow> = {}): SessionRow {
    const seq = Math.max(this.nextSeq, Math.max(0, ...this.rows.map((r) => r.seq)) + 1);
    this.nextSeq = seq + 1;
    const now = Date.now();
    const row: SessionRow = { id: `s-${seq}`, name: `session ${seq}`, seq, created: now, lastActive: now, archived: false, model, ...extra };
    this.rows.push(row);
    this.save();
    return { ...row };
  }
  bySeq(seq: number): SessionRow | undefined {
    return this.rows.find((r) => r.seq === seq);
  }
  children(seq: number): SessionRow[] {
    return this.rows.filter((r) => r.parent === seq).sort((a, b) => a.seq - b.seq);
  }
  logFile(row: SessionRow): string {
    return path.join(this.dir, "sessions", `${row.seq}.jsonl`);
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
  /** The thread's id, refused when a row of another kind or workspace already holds it. Writes nothing, so a caller can check before its write guard. */
  threadId(t: { kind: "workspace" | "ephemeral"; workspace: string; eph?: string }): string {
    const id = t.kind === "workspace" ? `w-${t.workspace}` : `e-${t.eph}`;
    const have = this.get(id);
    if (have && (have.kind !== t.kind || have.workspace !== t.workspace)) throw new Error(`${have.kind ?? "bench"} ${t.eph ?? t.workspace} belongs to ${have.workspace ?? "the bench"}`);
    return id;
  }
  /**
   * `w-{ws}` or `e-{eph}`; an id already listed comes back unchanged, but only for the same kind and workspace:
   * an ephemeral id is not scoped by its workspace, so a reuse under another would hand back the first one's file.
   * seq 0 keeps a thread out of create()'s numbering.
   */
  thread(t: { kind: "workspace" | "ephemeral"; workspace: string; eph?: string; target: string; file: string; model?: string }): SessionRow {
    const id = this.threadId(t);
    const have = this.get(id);
    if (have) return { ...have };
    const now = Date.now();
    const row: SessionRow = { id, name: t.kind === "workspace" ? t.workspace : `${t.workspace} · ${t.eph}`, seq: 0, file: t.file, created: now, lastActive: now, archived: false, model: t.model, kind: t.kind, workspace: t.workspace, target: t.target };
    this.rows.push(row);
    this.save();
    return { ...row };
  }
}
