import fs from "node:fs";
import path from "node:path";
import { ExchangeLog } from "./exchanges.ts";
import { Writable } from "./guard.ts";
import { Procs, Tasks } from "./ledger.ts";
import { Platform } from "./platform.ts";
import { setBackend, httpBackend } from "./engine/remote.ts";
import { append, readRows, type Row } from "./rows.ts";
import { Scheduler } from "./scheduler.ts";
import { Session, type Turn } from "./session.ts";
import { Subs } from "./sub.ts";
import { SessionList, type SessionRow } from "./sessions.ts";
import type { CapabilityAdapter, CapabilityRuntime } from "./operations/capabilities.ts";
import { createSharedAdapters, resolveWorkspaceProgress } from "./operations/adapters.ts";

export type BenchEvent = { type: string; [k: string]: unknown };
export type BenchOpts = { dir: string; readOnly: boolean; model: string; turn: Turn; platform?: Platform | ((method: string, p: string, body?: unknown) => Promise<{ status: number; data: unknown }>); extDir?: string };

/** Everything a person's list/rows page shows: rows plus a page total. `messages` is kept for the old electron UI. */
function page<T>(all: T[], after = 0, limit = all.length): { rows: T[]; messages: T[]; total: number } {
  const rows = all.slice(after, after + limit);
  return { rows, messages: rows, total: all.length };
}

// A workspace or ephemeral id becomes a path segment: a DNS label, like the object it names.
const WS_ID = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
/** Only bench (top) sessions count as "an open session"; a workspace thread never stands in for one. */
const isBench = (s: SessionRow) => (s.kind ?? "bench") === "bench";

/**
 * One person's bench in one team: the session tree runs on the sys-1 engine
 * (Scheduler/Session/Subs), and this object is the only writer of the folder's
 * harness files. Every device is a view of this object.
 */
export class Bench {
  readonly sessions: SessionList;
  readonly exchanges: ExchangeLog;
  readonly tasks: Tasks;
  readonly procs: Procs;
  readonly writable: Writable;
  readonly sched: Scheduler;
  readonly subs: Subs;
  private opts: BenchOpts;
  private listeners = new Set<(ev: BenchEvent) => void>();
  /** A person-facing `ask_user` with no parent: resolved by the next `send()` on that session instead of another user row. */
  private asks = new Map<number, (answer: string) => void>();

  constructor(opts: BenchOpts) {
    this.opts = opts;
    fs.mkdirSync(path.join(opts.dir, "sessions"), { recursive: true });
    this.sessions = new SessionList(opts.dir);
    this.exchanges = new ExchangeLog(opts.dir);
    this.tasks = new Tasks(opts.dir);
    this.procs = new Procs(opts.dir);
    this.writable = new Writable(opts.dir, (ok, reason) => this.emit({ type: "writable", ok, reason }));
    this.sched = new Scheduler(this.sessions, (row) => this.make(row));
    this.subs = new Subs(this.sessions, this.sched, opts.platform ?? (undefined as unknown as Platform), (ws) => this.backendFor(ws));
  }

  get readOnly(): boolean {
    return this.opts.readOnly;
  }

  get model(): string {
    return this.opts.model;
  }

  #capabilityRuntime?: Promise<CapabilityRuntime>;
  get capabilityRuntime(): Promise<CapabilityRuntime> {
    return (this.#capabilityRuntime ??= import("./operations/capabilities.ts").then((m) => m.createBenchCapabilityRuntime(this.procs, this.platformCapabilityAdapters())));
  }

  /**
   * `/v1`: the one platform call the bench makes for itself, mirrored as `{status, data}` for the
   * operations executor's adapters (they read a status code, not a thrown error).
   */
  private v1(method: string, p: string, body?: unknown): Promise<{ status: number; data: unknown }> {
    const platform = this.opts.platform;
    if (!platform) return Promise.resolve({ status: 503, data: { error: "no platform configured" } });
    // Tests wire a plain `(method, route, body) => {status, data}` function in place of a real Platform.
    if (typeof platform === "function") return (platform as (m: string, p: string, b?: unknown) => Promise<{ status: number; data: unknown }>)(method, p, body);
    return platform.raw(method, p, body);
  }

  private platformCapabilityAdapters(): Readonly<Record<string, CapabilityAdapter>> {
    const platform = (method: string, route: string, body?: unknown) => this.v1(method, route, body);
    const bench = async (method: string, route: string, body?: unknown) => {
      if (method !== "GET" || body !== undefined) return { ok: false, data: { error: "unsupported bench read" } };
      if (route === "/procs") return { ok: true, data: this.procs.all() };
      // Exchanges/asks are dropped; workspace.progress still asks for them, so answer empty rather than "unsupported".
      if (route.startsWith("/exchanges?workspace=")) return { ok: true, data: [] };
      const match = /^\/workspaces\/([^/]+)\/messages\?limit=10$/.exec(route);
      if (match) {
        const workspace = decodeURIComponent(match[1]);
        const session = this.sessions.all().find((row) => row.workspace === workspace || row.target === workspace || row.id === `w-${workspace}`);
        return session ? { ok: true, data: await this.messages(session.id, undefined, 10) } : { ok: true, data: { messages: [] } };
      }
      return { ok: false, data: { error: "unsupported bench read" } };
    };
    return {
      ...createSharedAdapters({ platform, bench }),
      "workspace.progress": resolveWorkspaceProgress(platform, bench),
    };
  }

  onEvent(fn: (ev: BenchEvent) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }
  private emit(ev: BenchEvent) {
    for (const fn of this.listeners) fn(ev);
  }
  private write<T>(fn: () => T): T | undefined {
    try {
      return this.writable.run(fn);
    } catch {
      return undefined; // the guard has flipped and said so on the bus
    }
  }

  private async backendFor(ws: string): Promise<void> {
    if (!this.opts.platform) return; // no platform (tests): leave the backend unset
    setBackend(ws, httpBackend(await this.opts.platform.tools(ws)));
  }

  private make(row: SessionRow): Session {
    const hooks = {
      delegate: (s: Session, t: string, i: string) => this.subs.delegate(s, t, i),
      tell: (s: Session, m: string) => this.emit({ type: "row", session: s.row.id, row: { kind: "tell", text: m } }),
      askPerson: (s: Session, q: string) => this.askPerson(s, q),
    };
    const s = new Session(row, this.sessions.logFile(row), this.opts.turn, hooks, (r) => this.emit({ type: "row", session: row.id, row: r }));
    // The scheduler (and Subs.delegate/finish) set onAnswer on every session it makes; a sub's
    // answer must always go through Subs.finish (push-back, clone delete), even across a
    // bench restart, so the setter is pinned rather than left to whoever assigns last.
    if (row.tier === "sub") Object.defineProperty(s, "onAnswer", { get: () => (c: Session, e: Extract<Row, { kind: "turn.end" }>) => this.subs.finish(c, e), set: () => {} });
    // A main never carries `parent` (Subs.toMain is delegation, not parentage), so the scheduler's
    // default parent-only routing has nowhere to deliver a main's answer. Route it to whichever
    // session(s) sent the `user` rows this turn actually answered, read back from the log itself.
    if (row.tier === "main") Object.defineProperty(s, "onAnswer", { get: () => (c: Session, e: Extract<Row, { kind: "turn.end" }>) => this.deliverMain(c, e), set: () => {} });
    return s;
  }

  private deliverMain(child: Session, end: Extract<Row, { kind: "turn.end" }>) {
    if (end.answer === undefined) return;
    const rows = readRows(child.file);
    // The user rows a turn answers are the ones between the previous turn.end (or the log start)
    // and this turn's own turn.start — unread() in rows.ts derives a turn's input the same way.
    const start = rows.findIndex((r) => r.kind === "turn.start" && r.turn === end.turn);
    let from0 = 0;
    for (let i = start - 1; i >= 0; i--) if (rows[i].kind === "turn.end") { from0 = i + 1; break; }
    const senders = new Set(rows.slice(from0, start).filter((r) => r.kind === "user" && typeof r.from === "number").map((r) => (r as { from: number }).from));
    for (const from of senders) this.sched.get(from)?.receive(child.row.seq, end.answer, end.turn);
    this.sched.kick();
  }

  private askPerson(s: Session, question: string): Promise<string> {
    if (s.row.parent !== undefined) {
      // The child's turn ends now; the parent's next delegate to it carries the answer (spec §4/§5).
      const parent = this.sessions.bySeq(s.row.parent)!;
      this.sched.get(parent.seq)!.receive(s.row.seq, question);
      this.sched.kick();
      return Promise.resolve("asked the parent; its answer arrives as a message");
    }
    return new Promise((resolve) => {
      this.asks.set(s.row.seq, resolve);
      // The brief mandates the row; the event alone is not the record.
      const row: Row = { kind: "turn.step", ts: Date.now(), turn: s.current ?? -1, step: `ask:${question}` };
      append(s.file, row);
      this.emit({ type: "row", session: s.row.id, row });
    });
  }

  async start(): Promise<void> {
    if (this.opts.readOnly) return;
    for (const row of this.write(() => this.tasks.markLost()) ?? []) this.emit({ type: "task", row });
    if (this.write(() => this.procs.markLost())?.length) this.emit({ type: "procs", rows: this.procs.all() });
    for (const s of this.sessions.all().filter((x) => x.tier === "main" && x.workspace)) await this.backendFor(s.workspace!);
    this.sched.boot();
  }

  async stop(): Promise<void> {
    for (const s of this.sched.sessions.values()) s.abort();
  }

  /** What the idle clock asks: is anything running that a client leaving must not stop? */
  busy(): boolean {
    return (
      [...this.sched.sessions.values()].some((s) => s.running) ||
      this.tasks.all().some((t) => t.state === "running" || t.state === "background") ||
      this.procs.all().some((p) => p.ended === undefined)
    );
  }

  private refuse(write: boolean) {
    if (this.opts.readOnly) throw new Error("this bench is read-only: you are no longer in this team, so it reads history and runs nothing");
    if (write && !this.writable.ok()) throw new Error(`the bench folder is not writable: ${this.writable.reason()}; prompts are refused until it is`);
  }

  async create(tier: "top" | "main" = "main"): Promise<SessionRow> {
    this.refuse(true);
    if (tier === "top" && this.sessions.all().some((s) => !s.archived && s.tier === "top")) throw new Error("this bench already has a top session");
    const s = this.writable.run(() => this.sessions.create(this.opts.model, { tier, state: "open", kind: tier === "top" ? "bench" : "workspace" }));
    this.sched.kick();
    this.emit({ type: "sessions" });
    return s;
  }

  async send(id: string, text: string): Promise<{ turn: number }> {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    this.refuse(true);
    if (s.archived) throw new Error(`session ${id} is archived; restore it to send`);
    if (s.state === "closed") throw new Error(`session ${id} is closed`);
    const resolve = this.asks.get(s.seq);
    if (resolve) { this.asks.delete(s.seq); resolve(text); }
    else (this.sched.get(s.seq) ?? this.make(s)).receive("person", text);
    if (text.trim() && !text.startsWith("/") && s.name === `session ${s.seq}`) {
      this.write(() => this.sessions.update(id, { name: text.replace(/\s+/g, " ").slice(0, 40), lastActive: Date.now() }));
      this.emit({ type: "sessions" });
    }
    this.sched.kick();
    const rows = readRows(this.sessions.logFile(s));
    return { turn: 1 + rows.reduce((m, r) => ("turn" in r ? Math.max(m, r.turn) : m), 0) };
  }

  async abort(id: string): Promise<void> {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    this.sched.get(s.seq)?.abort();
  }

  async children(id: string): Promise<SessionRow[]> {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    return this.sessions.children(s.seq);
  }

  async rows(id: string, after?: number, limit?: number): Promise<{ rows: unknown[]; messages: unknown[]; total: number }> {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    return page(readRows(this.sessions.logFile(s)), after, limit);
  }
  /** Kept for the old electron UI, same shape as rows(). */
  async messages(id: string, after?: number, limit?: number) {
    return this.rows(id, after, limit);
  }
  /** A workspace's main has a minted seq now, not the old `w-{ws}` id — resolve by workspace name. */
  async workspaceMessages(ws: string, after?: number, limit?: number) {
    const s = this.sessions.all().find((r) => r.kind === "workspace" && r.workspace === ws);
    return s ? this.rows(s.id, after, limit) : { rows: [], messages: [], total: 0 };
  }

  async archive(id: string): Promise<SessionRow> {
    this.refuse(true);
    const have = this.sessions.get(id);
    if (!have) throw new Error(`no session ${id}`);
    if (isBench(have) && this.sessions.all().filter((s) => !s.archived && isBench(s)).length < 2) throw new Error("this is the only open session; start another before archiving it");
    this.sched.get(have.seq)?.abort();
    const s = this.writable.run(() => this.sessions.update(id, { archived: true }));
    this.emit({ type: "sessions" });
    return s;
  }

  async restore(id: string): Promise<SessionRow> {
    this.refuse(true);
    if (!this.sessions.get(id)) throw new Error(`no session ${id}`);
    const s = this.writable.run(() => this.sessions.update(id, { archived: false, lastActive: Date.now() }));
    this.sched.kick();
    this.emit({ type: "sessions" });
    return s;
  }

  async openWorkspace(ws: string): Promise<SessionRow> {
    return this.openThread("workspace", ws);
  }

  async openEphemeral(ws: string, eph: string): Promise<SessionRow> {
    return this.openThread("ephemeral", ws, eph);
  }

  private async openThread(kind: "workspace" | "ephemeral", ws: string, eph?: string): Promise<SessionRow> {
    for (const x of kind === "workspace" ? [ws] : [ws, eph]) if (typeof x !== "string" || !WS_ID.test(x)) throw new Error(`not a workspace id: ${x}`);
    this.refuse(true);
    // sessions.thread() mints seq 0 for every thread — fine for an ephemeral's own history file, but a
    // main needs a real seq: it is a sys-1 tree node the scheduler indexes by seq and Subs.toMain
    // targets by workspace name, and every workspace sharing seq 0 would share one log file too.
    let s: SessionRow;
    if (kind === "workspace") {
      const existing = this.sessions.all().find((r) => r.kind === "workspace" && r.workspace === ws);
      s = existing ?? this.writable.run(() => this.sessions.create(this.opts.model, { name: ws, kind: "workspace", workspace: ws, target: ws, tier: "main", state: "open" }));
    } else {
      this.sessions.threadId({ kind, workspace: ws, eph });
      const base = path.join(this.opts.dir, "workspaces", ws);
      const file = path.join(base, "eph", `${eph}.jsonl`);
      s = this.writable.run(() => {
        fs.mkdirSync(path.dirname(file), { recursive: true });
        return this.sessions.thread({ kind, workspace: ws, eph, target: eph!, file, model: this.opts.model });
      });
    }
    if (s.archived) throw new Error(`session ${s.id} is archived; restore it to send`);
    await this.backendFor(ws);
    this.sched.kick();
    this.emit({ type: "sessions" });
    return this.sessions.get(s.id)!;
  }

  private inFlight(id: string): string[] {
    return [
      ...this.tasks.all().filter((t) => t.session === id && (t.state === "running" || t.state === "background")).map((t) => `${t.tool} ${t.arg}`),
      ...this.procs.all().filter((p) => p.session === id && p.ended === undefined).map((p) => `process ${p.name}`),
    ];
  }

  async remove(id: string, stop: boolean): Promise<void> {
    this.refuse(true);
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    const items = this.inFlight(id);
    if (items.length && !stop) throw new Error(`in flight: ${items.join(", ")}`);
    this.sched.get(s.seq)?.abort();
    this.sched.sessions.delete(s.seq);
    // A sub's own open clone is left as is: deleting a workspace is a person's explicit act, not an
    // automatic side effect of removing the session that happened to be working on it.
    this.writable.run(() => {
      for (const t of this.tasks.all().filter((t) => t.session === id && (t.state === "running" || t.state === "background"))) this.tasks.transition({ id: t.id, state: "cancelled", ended: Date.now() });
      this.exchanges.discard(id);
      fs.rmSync(path.join(this.opts.dir, "btw", id), { recursive: true, force: true });
      const file = this.sessions.logFile(s);
      if (fs.existsSync(file)) {
        const trash = path.join(this.opts.dir, "sessions", ".trash");
        fs.mkdirSync(trash, { recursive: true });
        fs.renameSync(file, path.join(trash, !isBench(s) ? `${id}-${path.basename(file)}` : path.basename(file)));
      }
      if (s.workspace) {
        const ws = path.join(this.opts.dir, "workspaces", s.workspace);
        for (const d of [path.join(ws, "eph"), ws]) try { fs.rmdirSync(d); } catch { /* not empty or already gone */ }
      }
      // Never zero bench (top) sessions: the replacement takes a fresh id, never the removed one.
      if (!this.sessions.all().some((x) => !x.archived && x.id !== id && isBench(x))) this.sessions.create(this.opts.model, { tier: "top", state: "open", kind: "bench" });
      this.sessions.remove(id);
    });
    this.sched.kick();
    this.emit({ type: "sessions" });
    this.emit({ type: "procs", rows: this.procs.all() });
  }

}
