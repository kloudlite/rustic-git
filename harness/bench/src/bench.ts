import fs from "node:fs";
import path from "node:path";
import { ExchangeLog, type Exchange } from "./exchanges.ts";
import { Writable } from "./guard.ts";
import { Procs, Tasks, type ProcRow } from "./ledger.ts";
import { page, transcript } from "./reader.ts";
import { RpcChild, type PiEvent } from "./rpc-child.ts";
import { SessionList, type SessionRow } from "./sessions.ts";

export type BenchEvent = { type: string; [k: string]: unknown };
export type BenchOpts = { dir: string; readOnly: boolean; model: string; bin?: string; extDir?: string };

const TOOL: Record<string, string> = { bash: "Bash", read: "Read", write: "Write", edit: "Edit", grep: "Grep", glob: "Glob", ls: "List" };
const argOf = (name: string, args: Record<string, unknown>) =>
  name === "bash" ? String(args.command ?? "") : String(args.path ?? args.file_path ?? args.pattern ?? JSON.stringify(args)).slice(0, 200);

/**
 * One person's bench in one team: the list, a pi per open session, and the
 * logs beside them. The only writer of the folder's harness files; pi writes
 * its own session JSONL. Every device is a view of this object.
 */
export class Bench {
  readonly sessions: SessionList;
  readonly exchanges: ExchangeLog;
  readonly tasks: Tasks;
  readonly procs: Procs;
  readonly writable: Writable;
  private opts: BenchOpts;
  private children = new Map<string, RpcChild>();
  private listeners = new Set<(ev: BenchEvent & { pi?: string }) => void>();
  /** Sessions between agent_start and agent_end: a turn nobody watches still holds the bench up. */
  private turning = new Set<string>();

  constructor(opts: BenchOpts) {
    this.opts = opts;
    fs.mkdirSync(path.join(opts.dir, "sessions"), { recursive: true });
    this.sessions = new SessionList(opts.dir);
    this.exchanges = new ExchangeLog(opts.dir);
    this.tasks = new Tasks(opts.dir);
    this.procs = new Procs(opts.dir);
    this.writable = new Writable(opts.dir, (ok, reason) => this.emit({ type: "writable", ok, reason }));
  }

  onEvent(fn: (ev: BenchEvent & { pi?: string }) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }
  private emit(ev: BenchEvent & { pi?: string }) {
    for (const fn of this.listeners) fn(ev);
  }
  private write<T>(fn: () => T): T | undefined {
    try {
      return this.writable.run(fn);
    } catch {
      return undefined; // the guard has flipped and said so on the bus
    }
  }

  async start(): Promise<void> {
    if (this.opts.readOnly) return;
    // A new process holds none of the old one's children.
    for (const row of this.write(() => this.tasks.markLost()) ?? []) this.emit({ type: "task", row });
    if (this.write(() => this.procs.markLost())?.length) this.emit({ type: "procs", rows: this.procs.all() });
    if (!this.sessions.all().some((s) => !s.archived)) this.write(() => this.sessions.create(this.opts.model));
    for (const s of this.sessions.all().filter((x) => !x.archived)) this.open(s);
  }

  stop(): void {
    for (const c of this.children.values()) c.stop();
    this.children.clear();
  }

  private open(s: SessionRow): RpcChild {
    let c = this.children.get(s.id);
    if (c?.running()) return c;
    const file = s.file && fs.existsSync(s.file) ? s.file : undefined;
    const child: RpcChild = new RpcChild(s.id, { dir: path.join(this.opts.dir, "sessions"), file, model: s.model ?? this.opts.model, bin: this.opts.bin, extDir: this.opts.extDir }, (ev) => this.fold(s.id, child, ev));
    this.children.set(s.id, child);
    child.start();
    // The file name is pi's to choose; ask once so the list can reopen it.
    void child.send({ type: "get_state" }).catch(() => undefined);
    return child;
  }

  private fold(id: string, child: RpcChild, ev: PiEvent) {
    // A stopped child (archived, removed, replaced, or the bench stopping) still
    // reports its exit late; folded, it would mark its successor's tasks lost.
    if (this.children.get(id) !== child) return;
    if (this.sessions.get(id)) this.foldRow(id, ev);
    this.emit({ ...ev, pi: id });
  }

  private foldRow(id: string, ev: PiEvent) {
    const now = Date.now();
    const data = ev.data as { sessionFile?: string } | undefined;
    if (ev.type === "response" && typeof data?.sessionFile === "string" && this.sessions.get(id)?.file !== data.sessionFile) {
      this.write(() => this.sessions.update(id, { file: data.sessionFile }));
      this.emit({ type: "sessions" });
    }
    // These answer only {cancelled} (rpc.md): the new file has to be asked for.
    if (ev.type === "response" && (ev.command === "new_session" || ev.command === "switch_session") && ev.success) {
      void this.children.get(id)?.send({ type: "get_state" }).catch(() => undefined);
    }
    if (ev.type === "agent_start") {
      this.turning.add(id);
      this.write(() => this.sessions.update(id, { lastActive: now }));
    }
    if (ev.type === "agent_end") this.turning.delete(id);
    if (ev.type === "exit") {
      this.turning.delete(id);
      // Only this session's pi went; the others still hold their commands.
      for (const row of this.write(() => this.tasks.markLost(id)) ?? []) this.emit({ type: "task", row });
    }
    if (ev.type === "tool_execution_start") {
      const name = ev.toolName as string;
      const row = this.write(() => this.tasks.transition({ id: ev.toolCallId as string, session: id, tool: TOOL[name] ?? name, arg: argOf(name, (ev.args ?? {}) as Record<string, unknown>), state: "running", started: now }));
      if (row) this.emit({ type: "task", row });
    }
    if (ev.type === "tool_execution_end") {
      const out = ((ev.result as { content?: { text?: string }[] } | undefined)?.content ?? []).map((c) => c.text ?? "").join("");
      const bg = /^Sent to the background as task #(\d+)/.exec(out);
      const row = this.write(() => this.tasks.transition(bg ? { id: ev.toolCallId as string, n: Number(bg[1]), state: "background" } : { id: ev.toolCallId as string, state: ev.isError ? "failed" : "done", ended: now }));
      if (row) this.emit({ type: "task", row });
    }
    const m = ev.message as { role?: string; customType?: string; content?: string } | undefined;
    if (ev.type === "message_end" && m?.role === "custom" && m.customType === "background-task" && typeof m.content === "string") {
      const n = Number(/#(\d+)/.exec(m.content)?.[1]);
      const t = this.tasks.all().find((x) => x.session === id && x.n === n);
      const row = t && this.write(() => this.tasks.transition({ id: t.id, state: /exit [1-9]/.test(m.content!.split("\n")[0]) ? "failed" : "done", ended: now }));
      if (row) this.emit({ type: "task", row });
    }
    if (ev.type === "extension_ui_request" && ev.method === "setWidget") {
      const line = (ev.widgetLines as string[] | undefined)?.[0];
      try {
        if (ev.widgetKey === "harness:procs") {
          const rows = (line ? JSON.parse(line) : []) as Omit<ProcRow, "session">[];
          this.write(() => this.procs.snapshot(id, rows));
          this.emit({ type: "procs", rows: this.procs.all() });
        }
        if (ev.widgetKey === "harness:exchange" && line) {
          const x = JSON.parse(line) as Partial<Exchange> & { id: string };
          const known = this.exchanges.bySession(id).some((e) => e.id === x.id);
          if (known && x.state) this.write(() => this.exchanges.transition(x.id, x.state!));
          else if (x.workspace && x.dir) this.write(() => this.exchanges.record({ id: x.id, session: id, workspace: x.workspace!, dir: x.dir!, text: x.text ?? "", state: x.state ?? "sent", ref: x.ref }));
          this.emit({ type: "exchange", row: this.exchanges.bySession(id).find((e) => e.id === x.id) });
        }
      } catch {
        /* a widget line that is not ours */
      }
    }
  }

  /** What the idle clock asks: is anything running that a client leaving must not stop? */
  busy(): boolean {
    return (
      this.turning.size > 0 ||
      this.tasks.all().some((t) => t.state === "running" || t.state === "background") ||
      this.procs.all().some((p) => p.ended === undefined)
    );
  }

  private refuse(write: boolean) {
    if (this.opts.readOnly) throw new Error("this bench is read-only: you are no longer in this team, so it reads history and runs nothing");
    if (write && !this.writable.ok()) throw new Error(`the bench folder is not writable: ${this.writable.reason()}; prompts are refused until it is`);
  }

  async create(): Promise<SessionRow> {
    this.refuse(true);
    const s = this.writable.run(() => this.sessions.create(this.opts.model));
    this.open(s);
    this.emit({ type: "sessions" });
    return s;
  }

  async rpc(id: string, cmd: Record<string, unknown>): Promise<PiEvent> {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    this.refuse(cmd.type === "prompt" || cmd.type === "steer" || cmd.type === "follow_up");
    if (s.archived) throw new Error(`session ${id} is archived; restore it to send`);
    const msg = typeof cmd.message === "string" ? cmd.message.trim() : "";
    if (cmd.type === "prompt" && msg && !msg.startsWith("/") && s.name === `session ${s.seq}`) {
      this.write(() => this.sessions.update(id, { name: msg.replace(/\s+/g, " ").slice(0, 40), lastActive: Date.now() }));
      this.emit({ type: "sessions" });
    }
    return this.open(s).send(cmd);
  }

  async messages(id: string, after?: number, limit?: number): Promise<{ messages: unknown[]; total: number }> {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    const c = this.children.get(id);
    if (c?.running()) {
      const r = await c.send({ type: "get_messages" });
      return page((r.data as { messages?: unknown[] } | undefined)?.messages ?? [], after, limit);
    }
    return page(s.file && fs.existsSync(s.file) ? transcript(s.file) : [], after, limit);
  }

  async archive(id: string): Promise<SessionRow> {
    this.refuse(true);
    if (this.sessions.all().filter((s) => !s.archived).length < 2) throw new Error("this is the only open session; start another before archiving it");
    this.children.get(id)?.stop();
    this.children.delete(id);
    const s = this.writable.run(() => this.sessions.update(id, { archived: true }));
    this.emit({ type: "sessions" });
    return s;
  }

  async restore(id: string): Promise<SessionRow> {
    this.refuse(true);
    const s = this.writable.run(() => this.sessions.update(id, { archived: false, lastActive: Date.now() }));
    this.open(s);
    this.emit({ type: "sessions" });
    return s;
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
    const c = this.children.get(id);
    if (c?.running()) {
      // Commands and processes run in their own process groups: stop them
      // through pi before pi goes, or they outlive the session.
      for (const t of this.tasks.all().filter((t) => t.session === id && (t.state === "running" || t.state === "background")))
        await c.send({ type: "prompt", message: t.state === "background" && t.n !== undefined ? `/cancel #${t.n}` : `/cancel ${t.id}` }).catch(() => undefined);
      for (const p of this.procs.all().filter((p) => p.session === id && p.ended === undefined)) await c.send({ type: "prompt", message: `/proc-stop ${p.id}` }).catch(() => undefined);
      await c.send({ type: "abort" }).catch(() => undefined);
      c.stop();
    }
    this.children.delete(id);
    this.turning.delete(id);
    // A write failing midway leaves the child stopped but the row kept: it reopens on the next start and delete can be retried.
    this.writable.run(() => {
      for (const t of this.tasks.all().filter((t) => t.session === id && (t.state === "running" || t.state === "background"))) this.tasks.transition({ id: t.id, state: "cancelled", ended: Date.now() });
      this.exchanges.discard(id);
      fs.rmSync(path.join(this.opts.dir, "btw", id), { recursive: true, force: true });
      if (s.file && fs.existsSync(s.file)) {
        const trash = path.join(this.opts.dir, "sessions", ".trash");
        fs.mkdirSync(trash, { recursive: true });
        fs.renameSync(s.file, path.join(trash, path.basename(s.file)));
      }
      // Never zero sessions: the replacement takes a fresh id (nextSeq), never the removed one.
      if (!this.sessions.all().some((x) => !x.archived && x.id !== id)) this.open(this.sessions.create(this.opts.model));
      this.sessions.remove(id);
    });
    this.emit({ type: "sessions" });
    this.emit({ type: "procs", rows: this.procs.all() });
  }
}
