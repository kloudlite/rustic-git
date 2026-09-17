import fs from "node:fs";
import path from "node:path";
import { ExchangeLog, type Exchange } from "./exchanges.ts";
import { Writable } from "./guard.ts";
import { Procs, Tasks, type ProcRow } from "./ledger.ts";
import { page, transcript } from "./reader.ts";
import { RpcChild, type ChildOpts, type PiEvent } from "./rpc-child.ts";
import { SessionList, type SessionRow } from "./sessions.ts";
import { readJson, replaceJson } from "./log.ts";

export type BenchEvent = { type: string; [k: string]: unknown };
/** One outstanding ask: the exchange it settles, who to answer, and whose workspace it is in. */
type Ask = { exchange: string; from: string; workspace: string };
export type BenchOpts = {
  dir: string;
  readOnly: boolean;
  model: string;
  bin?: string;
  extDir?: string;
  /** A workspace's tool server address. Tests pass a fake; the real one asks /v1, loaded lazily so the bench's own routes never pull the extension in. */
  resolveTools?: (ws: string) => Promise<string>;
};

const TOOL: Record<string, string> = { bash: "Bash", read: "Read", write: "Write", edit: "Edit", grep: "Grep", glob: "Glob", ls: "List" };
const argOf = (name: string, args: Record<string, unknown>) =>
  name === "bash" ? String(args.command ?? "") : String(args.path ?? args.file_path ?? args.pattern ?? JSON.stringify(args)).slice(0, 200);
/** How long a btw fork may run before it is stopped and the call rejects. */
const BTW_TIMEOUT_MS = 5 * 60_000;
// A workspace or ephemeral id becomes a path segment: a DNS label, like the object it names.
const WS_ID = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
/** Only bench sessions count as "an open session"; a workspace thread never stands in for one. */
const isBench = (s: SessionRow) => (s.kind ?? "bench") === "bench";
/** A session id the bench mints: never a path walk, never a btw fork. */
const SESSION_ID = /^(bench|s-\d+|[we]-[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)$/;
/** `file` is accepted and ignored: import always rewrites it to the copied file. */
const IMPORT_FIELDS = new Set(["id", "name", "seq", "created", "lastActive", "archived", "model", "kind", "workspace", "target", "file"]);

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
  private btwSeq = new Map<string, number>();
  /** Per workspace session, the asks it has been handed and not yet answered, oldest first. */
  private asked = new Map<string, Ask[]>();
  private askSeq = 0;

  constructor(opts: BenchOpts) {
    this.opts = opts;
    fs.mkdirSync(path.join(opts.dir, "sessions"), { recursive: true });
    this.sessions = new SessionList(opts.dir);
    this.exchanges = new ExchangeLog(opts.dir);
    this.tasks = new Tasks(opts.dir);
    this.procs = new Procs(opts.dir);
    this.writable = new Writable(opts.dir, (ok, reason) => this.emit({ type: "writable", ok, reason }));
  }

  get readOnly(): boolean {
    return this.opts.readOnly;
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
    if (!this.sessions.all().some((s) => !s.archived && isBench(s))) this.write(() => this.sessions.create(this.opts.model));
    for (const s of this.sessions.all().filter((x) => !x.archived)) this.open(s);
  }

  async stop(): Promise<void> {
    const done = [...this.children.values()].map((c) => c.stop());
    this.children.clear();
    await Promise.all(done);
  }

  /** What this row's pi would be spawned with — `undefined` for a row that cannot be opened at all. */
  private childOpts(s: SessionRow): ChildOpts | undefined {
    const thread = !isBench(s);
    // A thread row with no file (an old import, a hand edit) is skipped, never a crash-loop at boot.
    if (thread && !s.file) {
      console.error(`harness-bench: skipping ${s.id}: a ${s.kind} session with no file`);
      return undefined;
    }
    // pi creates a thread's file at the path it is given, so a thread's file need not exist yet.
    const file = thread ? s.file : s.file && fs.existsSync(s.file) ? s.file : undefined;
    const dir = thread ? path.dirname(s.file!) : path.join(this.opts.dir, "sessions");
    return { dir, file, tools: thread ? s.target : undefined, model: s.model ?? this.opts.model, bin: this.opts.bin, extDir: this.opts.extDir };
  }

  /**
   * What a session can call and where those calls run. Answered from the spawn options whether or
   * not its pi is up, because it is a property of the session, not of a process that happens to be
   * running — and a bench session must be answerable while it is idle, which is most of the time.
   */
  tools(id: string): ReturnType<RpcChild["hands"]> {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    const o = this.childOpts(s);
    if (!o) throw new Error(`no session ${id}`);
    return new RpcChild(id, o, () => {}).hands();
  }

  private open(s: SessionRow): RpcChild | undefined {
    let c = this.children.get(s.id);
    if (c?.running()) return c;
    const o = this.childOpts(s);
    if (!o) return undefined;
    const child: RpcChild = new RpcChild(s.id, o, (ev) => this.fold(s.id, child, ev));
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
      const a = this.asked.get(id)?.[0];
      if (a) this.transitionAsk(a, "running");
      this.turning.add(id);
      this.write(() => this.sessions.update(id, { lastActive: now }));
    }
    if (ev.type === "agent_end") {
      this.turning.delete(id);
      // `willRetry` means this run is not the answer yet — pi keeps going on its own.
      if (ev.willRetry !== true) void this.deliver(id, ev.messages as { role?: string; content?: unknown }[] | undefined);
    }
    if (ev.type === "exit") {
      this.turning.delete(id);
      // Its pi is gone: nothing it was handed will ever be answered.
      for (const a of this.asked.get(id) ?? []) this.transitionAsk(a, "failed");
      this.asked.delete(id);
      // Only this session's pi went; the others still hold their commands.
      for (const row of this.write(() => this.tasks.markLost(id)) ?? []) this.emit({ type: "task", row });
      if (this.write(() => this.procs.markLost(id))?.length) this.emit({ type: "procs", rows: this.procs.all() });
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

  private transitionAsk(a: { exchange: string; from: string; workspace: string }, state: string) {
    this.write(() => this.exchanges.transition(a.exchange, state));
    this.emit({ type: "exchange", row: this.exchanges.bySession(a.from).find((e) => e.id === a.exchange) });
  }

  /**
   * A workspace session finished a turn: the answer goes back into the session that asked for it,
   * so the person sees one conversation rather than having to watch the other tab.
   *
   * WHICH ask it answers is read off the turn itself: the `[ask <id> …]` message that started it,
   * or a `[reply <id>]` the answer opens with. A turn carrying neither answered nobody — the
   * person typing in that workspace's own tab — and leaves the queue alone. It holds a queue and works through
   * it in whatever order makes sense, so a turn that answers a specific one starts with
   * `[reply <exchange>]`; without that tag the oldest outstanding ask is the one being answered,
   * which is the ordinary case of a queue of one. Anything the model invents that is not an
   * outstanding exchange of this workspace is ignored, not routed.
   */
  private async deliver(id: string, ran?: { role?: string; content?: unknown }[]) {
    const queue = this.asked.get(id);
    if (!queue?.length) return;
    const said = (m?: { content?: unknown }) => (typeof m?.content === "string" ? m.content : (Array.isArray(m?.content) ? m!.content : []).map((c: { text?: string }) => c.text ?? "").join("")).trim();
    // `agent_end` carries the run's own messages, but not every build puts the prompt that started
    // it among them; the transcript always has it, and reading the TAIL of either is the same walk.
    let all = (ran ?? []) as { role?: string; content?: unknown }[];
    // Only from the transcript: an unanswered turn leaves no assistant message to bound the walk,
    // so an earlier ask still waiting would be read as part of this turn. The last prompt is the
    // one this answer belongs to, and that is all a fallback may claim.
    let lastPromptOnly = false;
    if (!all.some((m) => m.role === "user")) {
      lastPromptOnly = true;
      try {
        all = (((await this.children.get(id)?.send({ type: "get_messages" }))?.data as { messages?: unknown[] } | undefined)?.messages ?? []) as typeof all;
      } catch {
        /* the child went; the head still has to settle */
      }
    }
    // This turn and no earlier one: the last answer, and the messages between it and the answer
    // before it. Scanning the whole transcript would find every ask ever sent, and answer them all.
    const end = all.map((m) => m.role).lastIndexOf("assistant");
    const answer = said(all[end]);
    const turn = all.slice(0, end < 0 ? all.length : end).reverse();
    const stop = turn.findIndex((m) => m.role === "assistant");
    const before = turn.slice(0, stop < 0 ? turn.length : stop).filter((m) => m.role === "user");
    const asks = (lastPromptOnly ? before.slice(0, 1) : before).map((m) => /^\[ask (\S+)/.exec(said(m))?.[1]).filter((x): x is string => !!x);
    // WHICH ask this turn answered, and whether it answered one at all. A person typing in the
    // workspace's own tab ends a turn like any other, and popping the queue for it would mark
    // somebody's ask done with an answer to a different question.
    const replied = /\[reply ([^\]]+)\]/.exec(answer)?.[1];
    const named = [replied, ...asks].find((x) => x && queue.some((q) => q.exchange === x));
    if (!named && !asks.length) return;
    // The tag is the truth; FIFO is the fallback when a turn took an ask whose id it dropped, or
    // when several asks were merged into one run.
    const at = Math.max(0, queue.findIndex((x) => x.exchange === named));
    const [a] = queue.splice(at, 1);
    if (!queue.length) this.asked.delete(id);
    this.transitionAsk(a, answer ? "done" : "failed");
    const back = this.exchanges.record({ id: `${a.exchange}-in`, session: a.from, workspace: a.workspace, dir: "in", text: answer.slice(0, 2000), state: "done", ref: a.exchange });
    this.emit({ type: "exchange", row: back });
    // The asking session may be gone by now (removed, archived): an answer nobody is waiting for is dropped, not thrown.
    if (!answer || !this.sessions.get(a.from)) return;
    await this.send(a.from, `[from workspace ${a.workspace}] ${answer}`).catch(() => undefined);
  }

  /**
   * Stop a background process. It runs on a tool server — the session's own workspace, or, for a
   * bench session, the bench pod's own workspace container over loopback — so this is the harness
   * reaching the same place the tool did, never a command typed at the model.
   */
  async killProc(session: string, id: string): Promise<void> {
    const s = this.sessions.get(session);
    if (!s) throw new Error(`no session ${session}`);
    const resolve = this.opts.resolveTools ?? ((ws: string) => import("../../pi/workspace-tools.ts").then((m) => m.resolveFromApi(ws)));
    const at = s.target ? await resolve(s.target) : BENCH_TOOLS;
    const r = await fetch(`http://${at}/tools/process_kill`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ id }) });
    if (!r.ok) throw new Error(`process ${id}: the tool server answered ${r.status}`);
    const row = this.write(() => this.procs.transitionEnded(session, id));
    if (row) this.emit({ type: "procs", rows: this.procs.all() });
  }

  /** A prompt into a session that may be mid-turn: pi queues a follow-up rather than refusing. */
  private send(id: string, message: string): Promise<PiEvent> {
    return this.rpc(id, { type: this.turning.has(id) ? "follow_up" : "prompt", message });
  }

  /**
   * One session handing work to a workspace (owner, 2026-09-17: "it should send message to
   * workspace in the queue and it need to be processed there"). The workspace's OWN session does
   * it — created here if it has none — so the work happens where the hands and the history are,
   * and is visible in that workspace's tab rather than hidden inside the asking session.
   *
   * An ask is NEVER refused because that workspace is busy or already holds one (owner): it is
   * handed over as a follow-up, pi holds the queue, and the session works through them in its own
   * order. The tag carries the exchange and who asked, because the answer has to find its way back
   * to one of several senders and only the model knows which one it just answered.
   *
   * ponytail: no deadline — a turn that never ends leaves its exchange `running` until the child
   * exits. The upgrade is a timer per ask that fails the exchange and tells the asker so.
   */
  async ask(workspace: string, text: string, from: string): Promise<{ session: string; exchange: string; workspace: string; queued: number }> {
    this.refuse(true);
    if (typeof text !== "string" || !text.trim()) throw new Error("an ask needs something to do");
    const asker = this.sessions.get(from);
    if (!asker) throw new Error(`no session ${from}`);
    const s = await this.openWorkspace(workspace);
    const exchange = `ask-${++this.askSeq}-${Date.now().toString(36)}`;
    const row = this.write(() => this.exchanges.record({ id: exchange, session: from, workspace, dir: "out", text, state: "queued" }));
    this.emit({ type: "exchange", row });
    const queue = this.asked.get(s.id) ?? [];
    queue.push({ exchange, from, workspace });
    this.asked.set(s.id, queue);
    try {
      await this.send(s.id, `[ask ${exchange} from ${asker.name}] ${text}`);
    } catch (e) {
      this.asked.set(s.id, queue.filter((x) => x.exchange !== exchange));
      this.transitionAsk({ exchange, from, workspace }, "failed");
      throw e;
    }
    return { session: s.id, exchange, workspace, queued: queue.length };
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
    // `/proc-stop <id>` was an extension command when background commands ran in the bench pod;
    // they run on a tool server now, so the harness answers it itself and no model sees it.
    const stop = /^\/proc-stop (\S+)$/.exec(msg);
    if (cmd.type === "prompt" && stop) {
      await this.killProc(id, stop[1]);
      return { type: "response", command: "prompt", success: true } as PiEvent;
    }
    if (cmd.type === "prompt" && msg && !msg.startsWith("/") && s.name === `session ${s.seq}`) {
      this.write(() => this.sessions.update(id, { name: msg.replace(/\s+/g, " ").slice(0, 40), lastActive: Date.now() }));
      this.emit({ type: "sessions" });
    }
    const c = this.open(s);
    if (!c) throw new Error(`session ${id} has no file to open`);
    return c.send(cmd);
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
    // Checked before writable.run: an unknown id is a 404, never an unwritable folder.
    const have = this.sessions.get(id);
    if (!have) throw new Error(`no session ${id}`);
    if (isBench(have) && this.sessions.all().filter((s) => !s.archived && isBench(s)).length < 2) throw new Error("this is the only open session; start another before archiving it");
    this.children.get(id)?.stop();
    this.children.delete(id);
    const s = this.writable.run(() => this.sessions.update(id, { archived: true }));
    this.emit({ type: "sessions" });
    return s;
  }

  async restore(id: string): Promise<SessionRow> {
    this.refuse(true);
    if (!this.sessions.get(id)) throw new Error(`no session ${id}`);
    const s = this.writable.run(() => this.sessions.update(id, { archived: false, lastActive: Date.now() }));
    this.open(s);
    this.emit({ type: "sessions" });
    return s;
  }

  async openWorkspace(ws: string): Promise<SessionRow> {
    return this.openThread("workspace", ws);
  }

  async openEphemeral(ws: string, eph: string): Promise<SessionRow> {
    return this.openThread("ephemeral", ws, eph);
  }

  private openThread(kind: "workspace" | "ephemeral", ws: string, eph?: string): SessionRow {
    // Checked before either id becomes a path.
    for (const x of kind === "workspace" ? [ws] : [ws, eph]) if (typeof x !== "string" || !WS_ID.test(x)) throw new Error(`not a workspace id: ${x}`);
    this.refuse(true);
    // Outside writable.run: a refusal there would read as a folder that cannot be written.
    this.sessions.threadId({ kind, workspace: ws, eph });
    const base = path.join(this.opts.dir, "workspaces", ws);
    const file = eph === undefined ? path.join(base, "thread.jsonl") : path.join(base, "eph", `${eph}.jsonl`);
    const s = this.writable.run(() => {
      fs.mkdirSync(path.dirname(file), { recursive: true });
      // An ephemeral is a workspace cut for one agent: its tools run on its own tool server.
      return this.sessions.thread({ kind, workspace: ws, eph, target: eph ?? ws, file, model: this.opts.model });
    });
    if (s.archived) throw new Error(`session ${s.id} is archived; restore it to send`);
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
      for (const p of this.procs.all().filter((p) => p.session === id && p.ended === undefined)) await this.killProc(id, p.id).catch(() => undefined);
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
        // Every workspace thread's file is thread.jsonl: prefix the id so two never collide in the trash.
        fs.renameSync(s.file, path.join(trash, !isBench(s) ? `${id}-${path.basename(s.file)}` : path.basename(s.file)));
      }
      if (s.workspace) {
        const ws = path.join(this.opts.dir, "workspaces", s.workspace);
        // rmdir refuses a directory that still holds another thread, which is exactly when it must stay.
        for (const d of [path.join(ws, "eph"), ws]) try { fs.rmdirSync(d); } catch { /* not empty or already gone */ }
      }
      // Never zero bench sessions: the replacement takes a fresh id (nextSeq), never the removed one.
      if (!this.sessions.all().some((x) => !x.archived && x.id !== id && isBench(x))) this.open(this.sessions.create(this.opts.model));
      this.sessions.remove(id);
    });
    this.emit({ type: "sessions" });
    this.emit({ type: "procs", rows: this.procs.all() });
  }

  /** A one-question, read-only fork: no kl_* tools, no streaming — the answer replays on completion. */
  async btw(session: string, question: string, timeoutMs = BTW_TIMEOUT_MS): Promise<{ id: string; question: string; entries: unknown[]; at: number }> {
    this.refuse(true);
    const s = this.sessions.get(session);
    // The fork's read tools run here: on a thread they would read the bench pod, not the workspace.
    if (s && !isBench(s)) throw new Error("btw is only for bench sessions");
    if (!s?.file || !fs.existsSync(s.file)) throw new Error("this session has no file yet; say something first");
    const dir = path.join(this.opts.dir, "btw", session);
    // Taken synchronously, so two concurrent calls never share an id; the files seed it after a restart.
    const n = Math.max(this.btwSeq.get(session) ?? 0, fs.existsSync(dir) ? fs.readdirSync(dir).length : 0) + 1;
    this.btwSeq.set(session, n);
    const id = `btw-${n}`;
    const forkDir = path.join(this.opts.dir, "btw", ".forks", `${session}-${id}`);
    fs.mkdirSync(forkDir, { recursive: true });
    let done!: () => void;
    let timedOut = false;
    const ended = new Promise<void>((r) => (done = r));
    const child = new RpcChild(id, { dir: forkDir, fork: s.file, model: s.model ?? this.opts.model, bin: this.opts.bin }, (ev) => {
      this.emit({ ...ev, pi: id });
      if (ev.type === "agent_end" || ev.type === "exit") done();
    });
    child.start();
    const timer = setTimeout(() => {
      timedOut = true;
      done();
    }, timeoutMs);
    timer.unref?.();
    try {
      const before = ((await child.send({ type: "get_messages" })).data as { messages?: unknown[] } | undefined)?.messages?.length ?? 0;
      await child.send({ type: "prompt", message: question });
      await ended;
      if (timedOut) throw new Error(`btw timed out after ${timeoutMs}ms`);
      const all = ((await child.send({ type: "get_messages" })).data as { messages?: unknown[] } | undefined)?.messages ?? [];
      const answer = { id, question, entries: all.slice(before), at: Date.now() };
      this.writable.run(() => replaceJson(path.join(dir, `${id}.json`), answer));
      return answer;
    } finally {
      clearTimeout(timer);
      await child.stop();
      // The fork's session file is pi's scratch; the answer is kept under btw/{session}.
      fs.rmSync(forkDir, { recursive: true, force: true });
    }
  }

  listBtw(session: string): { id: string; question: string; entries: unknown[]; at: number }[] {
    if (!this.sessions.get(session)) throw new Error(`no session ${session}`);
    const dir = path.join(this.opts.dir, "btw", session);
    if (!fs.existsSync(dir)) return [];
    return fs.readdirSync(dir).filter((f) => f.endsWith(".json")).map((f) => readJson(path.join(dir, f), null)).filter((x) => x !== null)
      .sort((a, b) => (a as { at: number }).at - (b as { at: number }).at) as { id: string; question: string; entries: unknown[]; at: number }[];
  }

  /** Session files copy in once by name; rows merge idempotently; loose files copy in and get no row. */
  import(items: unknown, loose: unknown): { added: string[]; files: number } {
    this.refuse(true);
    // Validated whole before writable.run: a bad body is a 400, never an unwritable folder, and no field reaches sessions.json unchecked.
    const bad = (why: string) => new Error(`bad import: ${why}`);
    const str = (v: unknown) => typeof v === "string";
    const int = (v: unknown) => Number.isInteger(v) && (v as number) >= 0;
    if (!Array.isArray(items) || !Array.isArray(loose)) throw bad("items and loose must be arrays");
    for (const it of items) {
      if (!it || typeof it !== "object" || !str(it.name) || (it.content !== undefined && !str(it.content))) throw bad("an item needs a name and string content");
      const r = it.row as Record<string, unknown>;
      if (!r || typeof r !== "object") throw bad("an item needs a row");
      const extra = Object.keys(r).filter((k) => !IMPORT_FIELDS.has(k));
      if (extra.length) throw bad(`unknown row field ${extra.join(", ")}`);
      if (!str(r.id) || !SESSION_ID.test(r.id as string)) throw bad(`row id ${JSON.stringify(r.id)}`);
      if (!str(r.name) || !int(r.seq) || typeof r.archived !== "boolean" || !Number.isFinite(r.lastActive) || (r.created !== undefined && !Number.isFinite(r.created))) throw bad(`row ${r.id}: name, seq, archived, lastActive`);
      if (r.kind !== undefined && !["bench", "workspace", "ephemeral"].includes(r.kind as string)) throw bad(`row ${r.id}: kind`);
      for (const k of ["workspace", "target"]) if (r[k] !== undefined && !(str(r[k]) && WS_ID.test(r[k] as string))) throw bad(`row ${r.id}: ${k}`);
      if ((r.model !== undefined && !str(r.model)) || (r.file !== undefined && !str(r.file))) throw bad(`row ${r.id}: model, file`);
    }
    for (const f of loose) if (!f || typeof f !== "object" || !str(f.name) || !str(f.content)) throw bad("a loose file needs a name and content");
    const valid = items as { row: SessionRow; name: string; content?: string }[];
    const looseFiles = loose as { name: string; content: string }[];
    const dir = path.join(this.opts.dir, "sessions");
    let files = 0;
    const put = (name: string, content: string) => {
      const safe = path.basename(name);
      const to = path.join(dir, safe);
      if (!safe.endsWith(".jsonl") || fs.existsSync(to)) return to;
      fs.writeFileSync(to, content, { flag: "wx" });
      files++;
      return to;
    };
    return this.writable.run(() => {
      const rows = valid.map(({ row, name, content }) => ({ ...row, archived: !!row.archived, file: content !== undefined ? put(name, content) : undefined }));
      for (const f of looseFiles) put(f.name, f.content);
      const added = this.sessions.merge(rows);
      if (added.length) this.emit({ type: "sessions" });
      return { added, files };
    });
  }
}
