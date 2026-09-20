import fs from "node:fs";
import path from "node:path";
import { Architecture, CONTRACTS_BOUNCE, onReply } from "./architecture.ts";
import { ExchangeLog, type Exchange } from "./exchanges.ts";
import { Writable } from "./guard.ts";
import { Plans, Procs, Tasks, type PlanState, type ProcRow } from "./ledger.ts";
import { Memories, type Memory } from "./memory.ts";
import { brief, nudge, reduce, terse, type PlanEvent } from "./plan.ts";
import { keepPersonOrder, order, question as triageQuestion } from "./triage.ts";
import { page, transcript } from "./reader.ts";
import { RpcChild, type ChildOpts, type PiEvent } from "./rpc-child.ts";
import { SessionList, type SessionRow } from "./sessions.ts";
import { Defaults, type Triple } from "./defaults.ts";
import { allProviders } from "./providers.ts";
import { readJson, replaceJson } from "./log.ts";
import type { CapabilityAdapter, CapabilityRuntime } from "./operations/capabilities.ts";
import { createSharedAdapters, resolveWorkspaceProgress } from "./operations/adapters.ts";

export type BenchEvent = { type: string; [k: string]: unknown };

/**
 * A card is identified by the session that raised it AND the id its child minted: two children mint
 * the same tool call id, and one key for both collapsed two cards into one (D3).
 */
// `.` and `-` are what the routes' id charset allows, so a key is a legal path segment.
const proposalKey = (session: string, id: string) => `${session}.${id}`;

/**
 * Card text reads to a PERSON, so it says names. A card said "Deliver mongodb traffic in
 * env-2193… to workspace ws-9e16…" while its siblings used names (api-test-report R-D15): every
 * workspace and environment id in what a person is asked to agree to is replaced by the name when
 * one is known. Unknown ids are left exactly as they are — a wrong name is worse than an id.
 */
export function withNames(text: string, names: Record<string, string>): string {
  if (!text) return text;
  return text.replace(/\b(?:ws|env)-[0-9a-z]{4,}\b/gi, (id) => names[id] ?? id);
}

/** A card that already has an answer: the first stands, and the second is refused (D12). */
export class AlreadyAnswered extends Error {
  readonly answer: string;
  constructor(id: string, answer: string) {
    super(`proposal ${id} was already answered`);
    this.name = "AlreadyAnswered";
    this.answer = answer;
  }
}

/** A model pick the provider has never heard of: refused with 409, naming what it does carry. */
export class NoSuchModel extends Error {
  readonly known: string[];
  constructor(model: string, known: string[]) {
    super(`no such model ${JSON.stringify(model)}`);
    this.name = "NoSuchModel";
    this.known = known;
  }
}
/**
 * One outstanding ask: the exchange it settles, who to answer, and whose workspace it is in.
 * `workspace` is always the ID — it is a path segment, a session key and an env var — and `name`
 * is only what a person reads.
 */
type Ask = { exchange: string; from: string; workspace: string; name?: string; agent?: true };
export type BenchOpts = {
  dir: string;
  readOnly: boolean;
  model: string;
  bin?: string;
  extDir?: string;
  /** A workspace's tool server address. Tests pass a fake; the real one asks /v1, loaded lazily so the bench's own routes never pull the extension in. */
  resolveTools?: (ws: string) => Promise<string>;
  /** The team's workspaces, for turning a NAME into an id. Tests pass a fake; the real one asks /v1. */
  listWorkspaces?: () => Promise<{ id: string; name?: string; packages?: string[] }[]>;
  /** The connected environment's services, for the architecture document's first version (§24). */
  listServices?: () => Promise<{ name: string; image?: string; ports?: (number | { port?: number })[] }[]>;
  /** `/v1`, for the one thing the bench asks of it itself: a subagent's tree. Tests pass a fake. */
  platform?: (method: string, path: string, body?: unknown) => Promise<{ status: number; data: unknown }>;
};

/** Tool calls that change something: what makes a turn "work" rather than a look around. */
const CHANGES = new Set(["write", "edit", "patch", "bash", "process", "ask"]);

/** Tool calls that wait on a PERSON, not on work: never tasks, never lost, never timed. */
const WAITS = new Set(["question", "ask_close"]);

const TOOL: Record<string, string> = { bash: "Bash", read: "Read", write: "Write", edit: "Edit", grep: "Grep", glob: "Glob", ls: "List" };
const argOf = (name: string, args: Record<string, unknown>) =>
  name === "bash" ? String(args.command ?? "") : String(args.path ?? args.file_path ?? args.pattern ?? JSON.stringify(args)).slice(0, 200);
/** How often a session with something running is asked what is still running. */
/** A burst of arrivals is one ordering; a fork that thinks too long is not worth waiting for. */
const TRIAGE_DEBOUNCE_MS = 3_000;
const TRIAGE_TIMEOUT_MS = 60_000;
/** How full the window may get before the conversation is summarised and carried on. */
const COMPACT_AT = 0.8;
/**
 * The fallback when a provider reports no context window: compact at this many cumulative tokens.
 * 160k is under the smallest window any model we run has, so it is early rather than late — a
 * summary that was not needed costs one call; one that came too late costs the conversation.
 */
const COMPACT_TOKENS = 160_000;
/** How long an agent gets to stop its own turn before its child is taken away. */
const ABORT_WAIT_MS = 10_000;
/** How long a tree may take to appear before the dispatch is refused, and how often it is asked. */
const TREE_READY_MS = 60_000;
const TREE_POLL_MS = 500;
const PROC_POLL_MS = 10_000;
const UNREACHABLE_SWEEPS = 3;
/** At most this many matching lines per watch message: a watch is a signal, not a log pipe. */
const WATCH_MAX_LINES = 20;
/** How many times one watch may fire before it says so and stops (spec §3.9 rule 5). */
const WATCH_MAX_FIRES = 20;
/**
 * Deadlines, per state (spec §3.9 rule 2). Nothing waits forever, and nothing waits SILENTLY: the
 * person sees the age on the card and the session is told what happened to its ask.
 */
/** Queued longer than this and the workspace session never took it: redeliver once, then blocked. */
const ASK_PICKUP_MS = 60_000;
/** Running with no progress for this long: the workspace is asked one line, and the card says so. */
const ASK_IDLE_MS = 10 * 60_000;
/** No answer to that one line: the ask is expired and the asking session is told. */
const ASK_NUDGE_GRACE_MS = 2 * 60_000;
/** How often deadlines are advanced, orphans reaped and lost work marked. */
const SWEEP_MS = 30_000;
/** How long a sent prompt holds the seat when no turn is ever seen for it. */
const START_GRACE_MS = 5_000;
/**
 * A background task with nobody watching it: a job that has run this long without ending is
 * reported once as expired rather than sitting in the panel forever (spec §3.9 rule 8). It is not
 * killed — what it is doing may still be wanted; it stops being something a session waits on.
 */
const JOB_MAX_MS = 60 * 60_000;
/** How long a btw fork may run before it is stopped and the call rejects. */
const BTW_TIMEOUT_MS = 5 * 60_000;
/** A question about a workspace is a question, not a job: it answers or it does not. */
const INFO_TIMEOUT_MS = 2 * 60_000;
// A workspace or ephemeral id becomes a path segment: a DNS label, like the object it names.
const WS_ID = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
/** Only bench sessions count as "an open session"; a workspace thread never stands in for one. */
const isBench = (s: SessionRow) => (s.kind ?? "bench") === "bench";
/** A session id the bench mints: never a path walk, never a btw fork. */
const SESSION_ID = /^(bench|s-\d+|[we]-[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)$/;
/** `file` is accepted and ignored: import always rewrites it to the copied file. */
const IMPORT_FIELDS = new Set(["id", "name", "seq", "created", "lastActive", "archived", "model", "thinking", "effort", "kind", "workspace", "target", "tree", "file"]);

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
  /**
   * The capability runtime and the platform tool module (pi/kloudlite.ts) load on first use, not
   * at boot: this getter is what keeps operations/capabilities.ts's own import of pi/kloudlite.ts
   * out of the bench's static graph, which was otherwise the one chain the executor work added.
   */
  #capabilityRuntime?: Promise<CapabilityRuntime>;
  get capabilityRuntime(): Promise<CapabilityRuntime> {
    return (this.#capabilityRuntime ??= import("./operations/capabilities.ts").then((m) => m.createBenchCapabilityRuntime(this.procs, this.platformCapabilityAdapters())));
  }
  readonly plans: Plans;
  readonly memories: Memories;
  readonly defaults: Defaults;
  /** What runs where and what talks to what, for this space (§24). Shared by every session. */
  readonly architecture: Architecture;
  readonly writable: Writable;
  private opts: BenchOpts;
  private children = new Map<string, RpcChild>();
  private listeners = new Set<(ev: BenchEvent & { pi?: string }) => void>();
  /** Sessions between agent_start and agent_end: a turn nobody watches still holds the bench up. */
  private turning = new Set<string>();
  private btwSeq = new Map<string, number>();
  /** Exchanges already bounced for a missing `contracts:` line: asked once, never twice. */
  private bounced = new Set<string>();
  /** Per workspace session, the asks it has been handed and not yet answered, oldest first. */
  private asked = new Map<string, Ask[]>();
  private askSeq = 0;
  /** Sessions with a prompt sent whose turn has not been seen yet: the gap `turning` cannot cover. */
  private starting = new Set<string>();
  /** When this process started: an exchange older than it was left behind by a restart. */
  private readonly bootedAt = Date.now();
  /** Per exchange: when it entered its state, and whether it has already been redelivered or nudged. */
  private clocks = new Map<string, { at: number; redelivered?: true; nudgedAt?: number; card?: string }>();
  private sweeper?: ReturnType<typeof setInterval>;
  private procPoll?: ReturnType<typeof setInterval>;
  /** Consecutive sweeps a session's tool server could not be asked; three is "gone", not "a blip". */
  private unreachable = new Map<string, number>();
  /** Patterns a session asked to be told about, by process id. */
  private watching = new Map<string, { session: string; re: RegExp; since: number; sinceErr: number; pattern: string; said: Set<string>; fires: number }>();
  /** Tool calls in the turn a session is in, and whether it has already been nudged about this one. */
  private turnCalls = new Map<string, { calls: number; nudged?: true }>();
  /** The last thing a session SAID, so a turn that ended in a question is read as waiting. */
  private lastSaid = new Map<string, string>();
  /** Debounce per session: a burst of arrivals is one ordering, not one per message. */
  private triaging = new Map<string, ReturnType<typeof setTimeout>>();
  /** Sessions being summarised: one at a time, and never twice for the same growth. */
  private compacting = new Set<string>();
  /** Questions a session is holding: the extension waits on one, a person in the desktop answers it. */
  private proposals = new Map<string, { session: string; raw: string; tool: string; summary: string; preview?: string; args: unknown; question?: unknown; answer?: string; wake: (() => void)[] }>();

  constructor(opts: BenchOpts) {
    this.opts = opts;
    fs.mkdirSync(path.join(opts.dir, "sessions"), { recursive: true });
    this.sessions = new SessionList(opts.dir);
    this.exchanges = new ExchangeLog(opts.dir);
    this.tasks = new Tasks(opts.dir);
    this.procs = new Procs(opts.dir);
    this.plans = new Plans(opts.dir);
    this.memories = new Memories(opts.dir);
    this.defaults = new Defaults(opts.dir);
    this.architecture = new Architecture(opts.dir);
    this.writable = new Writable(opts.dir, (ok, reason) => this.emit({ type: "writable", ok, reason }));
  }

  private platformCapabilityAdapters(): Readonly<Record<string, CapabilityAdapter>> {
    const platform = (method: string, route: string, body?: unknown) => this.v1(method, route, body);
    const bench = async (method: string, route: string, body?: unknown) => {
      if (method !== "GET" || body !== undefined) return { ok: false, data: { error: "unsupported bench read" } };
      if (route === "/procs") return { ok: true, data: this.procs.all() };
      const match = /^\/workspaces\/([^/]+)\/messages\?limit=10$/.exec(route);
      if (match) {
        const workspace = decodeURIComponent(match[1]);
        const session = this.sessions.all().find((row) => row.workspace === workspace || row.target === workspace || row.id === `w-${workspace}`);
        return session ? { ok: true, data: await this.messages(session.id, undefined, 10) } : { ok: true, data: { messages: [] } };
      }
      if (route.startsWith("/exchanges?workspace=")) return { ok: true, data: this.exchanges.byWorkspace(decodeURIComponent(route.split("=")[1])) };
      return { ok: false, data: { error: "unsupported bench read" } };
    };
    return {
      ...createSharedAdapters({ platform, bench }),
      "workspace.progress": resolveWorkspaceProgress(platform, bench),
    };
  }

  get readOnly(): boolean {
    return this.opts.readOnly;
  }

  /** What answers here when a session names nothing of its own — the fleet's `KL_MODEL`. */
  get model(): string {
    return this.opts.model;
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
    // A process runs on the WORKSPACE's tool server, not inside pi: a bench restart says nothing
    // about it (the owner watched a live dev server marked "lost 13m"). The poll re-syncs instead,
    // in both directions — a row this ledger calls lost is revived if its tool server still has it.
    if (this.procs.all().length) this.pollProcs();
    // Open asks SURVIVE a restart. The queue lived only in memory, so a bench that restarted while
    // a workspace was working left the ask `running` in the log with nobody waiting on it and
    // nobody able to settle it — the asking session waited forever (spec §3.9 rule 1).
    if (!this.sessions.all().some((s) => !s.archived && isBench(s))) this.write(() => this.sessions.create({ model: this.opts.model }));
    // The architecture document starts with the machines in it (§24): an empty document is one
    // nobody writes, and one that already names the workspaces and services is one somebody
    // corrects. It is written once and never overwritten from here again.
    void this.seedArchitecture().catch(() => undefined);
    for (const s of this.sessions.all().filter((x) => !x.archived)) this.open(s);
    // AFTER the sessions are open, so a queued ask has somebody to be re-delivered to: this reads
    // the log and puts every open exchange back where it was, or ends it (spec §3.9 rule 1).
    this.resumeAsks();
    this.sweepOn();
  }

  /**
   * The queue of open asks, folded from `exchanges.jsonl` rather than remembered: the log is the
   * record and the map is a view of it (spec §3.9 rule 1). Every open exchange is re-checked on
   * boot, never assumed — a workspace session that is gone leaves its asks `blocked` rather than
   * waiting on a session that will never answer.
   */
  private resumeAsks(): void {
    const open = this.exchanges.active();
    for (const e of open) {
      const asker = this.sessions.get(e.session);
      const holder = this.sessions.all().find((s) => !s.archived && (s.workspace === e.workspace || s.target === e.workspace || s.id === `w-${e.workspace}`));
      // Nobody left to answer it, or nobody left to answer TO: the wait ends here rather than
      // outliving the thing it was for.
      if (!asker || asker.archived || !holder) {
        this.write(() => this.exchanges.transition(e.id, "blocked"));
        this.plan(e.session, { type: "ask_failed", exchange: e.id, task: e.workspace });
        this.emit({ type: "exchange", row: this.exchanges.bySession(e.session).find((x) => x.id === e.id) });
        continue;
      }
      const queue = this.asked.get(holder.id) ?? [];
      if (!queue.some((q) => q.exchange === e.id)) queue.push({ exchange: e.id, from: e.session, workspace: e.workspace, name: holder.name });
      this.asked.set(holder.id, queue);
      // The plan says what is outstanding, so a restart does not empty the panel a person reads.
      this.plan(e.session, { type: "asked", exchange: e.id, to: e.workspace, task: e.text });
      /**
       * A QUEUED ask was never taken by the workspace session, and the child that would have taken
       * it is gone with the restart: nothing is going to pick it up on its own. It sat queued
       * forever (api-test-report R-D19), so it is re-delivered here — once, and the deadline takes
       * it from there if that lands nowhere either.
       */
      /**
       * A restart takes the CHILD with it, so no exchange is mid-turn on the other side of a boot —
       * `running` means "a session was working on this", and after a restart nobody is. A pod
       * delete, a fleet roll and a head-of-queue ask each left one running for five minutes past
       * boot (api-test-report R-D19, three samples). Every non-terminal exchange is re-delivered
       * once, with its clock starting HERE, and the ordinary deadline takes it from there.
       */
      // Only what predates THIS process: `resumeAsks` also runs for a bench that never went down
      // (a session opened mid-life), and re-sending a live ask would hand the workspace the same
      // work twice.
      this.clocks.set(e.id, { at: Date.now() });
      if (e.ts >= this.bootedAt) continue;
      this.tell(holder.id, `[ask ${e.id} from ${e.session}] ${e.text}`);
      // Re-delivered already: the deadline must not spend its one redelivery on it a second time.
      this.clocks.set(e.id, { at: Date.now(), redelivered: true });
    }
  }

  /**
   * The bench goes; what it started stays. A background process runs on the WORKSPACE's tool
   * server, and it belongs to the workspace — an idle exit, a pod recreate or a restart says
   * nothing about it. The revive sweep re-adopts them on the next start.
   */
  async stop(): Promise<void> {
    clearInterval(this.procPoll);
    this.procPoll = undefined;
    clearInterval(this.sweeper);
    this.sweeper = undefined;
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
    const t = this.rowTriple(s);
    // `tree` is the session's, pinned on every ide call the child makes; `ephemeral` is what tells
    // an agent it is one. Both are properties of the ROW, so a bench restart re-spawns the child
    // into the same working directory rather than into the workspace's own.
    return { dir, file, tools: thread ? s.target : undefined, tree: s.tree, ephemeral: s.kind === "ephemeral", model: t.model ?? this.opts.model, thinking: t.thinking, effort: t.effort, bin: this.opts.bin, extDir: this.opts.extDir };
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

  /**
   * What this session answers with: its own fields, with the general default filling whatever it
   * does not name (spec §1.2, resolution order 1 then 2 — pi's own default is rung 3 and is what is
   * left when neither names a model).
   */
  rowTriple(s: SessionRow): Triple {
    const d = this.defaults.get();
    return { model: s.model ?? d.model, thinking: s.thinking ?? d.thinking, effort: s.effort ?? d.effort };
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
    // Every start re-applies the triple, so a restarted bench comes back on the same model without
    // the person noticing (spec §1.2). Fire-and-forget: a child that dies on start has nothing to set.
    void child.applyTriple(this.rowTriple(s)).catch(() => undefined);
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
      // A fresh session has a fresh FILE: until the row knows it, the next read of this session
      // replays the old transcript and `/clear` looks like it did nothing (owner, 2026-09-17).
      // The row is corrected below when get_state answers; the event is what makes every window
      // drop what it cached.
      void this.children.get(id)?.send({ type: "get_state" }).catch(() => undefined);
      // A fresh session has no plan either: the old one's items belonged to work that is no longer
      // in front of anybody (owner, 2026-09-17 — `/clear` left `[{"text":"","state":"done"}]`).
      this.write(() => this.plans.discard(id));
      this.emit({ type: "plan", session: id, items: [] });
      this.emit({ type: "cleared", session: id });
    }
    if (ev.type === "agent_start") {
      this.turnCalls.set(id, { calls: 0 });
      const a = this.asked.get(id)?.[0];
      if (a) this.transitionAsk(a, "running");
      this.turning.add(id);
      // The turn this prompt was waiting to see: the seat it held is now `turning`'s.
      this.starting.delete(id);
      this.write(() => this.sessions.update(id, { lastActive: now }));
    }
    if (ev.type === "agent_end") {
      this.turning.delete(id);
      /**
       * A turn that ended in an ERROR — the model refused, there is no key, a tool failed — answers
       * nobody, and the ask it was for would otherwise wait out its deadline before anybody heard
       * why (spec §3.9 rule 3). It settles `blocked` with the plain sentence instead.
       */
      // pi reports a PROVIDER refusal on the message, not as `error`: `stopReason: "error"` with
      // `errorMessage` ("400: The supported API model names are …"). Read only from `ev.error`, that
      // turn looked like an ordinary empty answer — the caller and the person got silence, and the
      // bench log was empty (api-test-report D1).
      const stopped = (ev.messages as { stopReason?: string; errorMessage?: string }[] | undefined)?.find((m) => m?.stopReason === "error" && m.errorMessage);
      const failed =
        typeof (ev as { error?: unknown }).error === "string"
          ? String((ev as { error?: string }).error)
          : stopped?.errorMessage
            ? String(stopped.errorMessage)
            : undefined;
      // A provider error is never silent: it reaches the person's transcript and the log.
      if (stopped?.errorMessage && ev.willRetry !== true) {
        console.error(`session ${id}: the model provider refused this turn: ${String(stopped.errorMessage).slice(0, 200)}`);
        this.emit({ type: "turn_error", session: id, text: String(stopped.errorMessage).split("\n")[0].slice(0, 200), pi: id });
      }
      if (failed && ev.willRetry !== true) {
        for (const a of this.asked.get(id) ?? []) this.settle({ id: a.exchange, session: a.from, workspace: a.workspace }, "blocked", failed.split("\n")[0].slice(0, 160));
        this.asked.delete(id);
        return;
      }
      // `willRetry` means this run is not the answer yet — pi keeps going on its own.
      if (ev.willRetry !== true) void this.deliver(id, ev.messages as { role?: string; content?: unknown }[] | undefined);
      if (ev.willRetry !== true) this.keepPlanCurrent(id);
      // Between turns is the only safe moment to summarise: mid-turn would rewrite what the model
      // is holding while it is using it.
      const u = (ev.usage ?? (ev as { data?: { usage?: unknown } }).data?.usage) as { totalTokens?: number; contextWindow?: number } | undefined;
      // A provider that reports no window still reports tokens: the fallback is a count, not a guess
      // at how big its window is.
      if (ev.willRetry !== true && u?.totalTokens) void this.compactIfFull(id, u.totalTokens, u.contextWindow);
    }
    if (ev.type === "exit") {
      this.turning.delete(id);
      // Its pi is gone: nothing it was handed will ever be answered.
      for (const a of this.asked.get(id) ?? []) this.transitionAsk(a, "failed");
      this.asked.delete(id);
      // Only this session's pi went; the others still hold their commands.
      for (const row of this.write(() => this.tasks.markLost(id)) ?? []) this.emit({ type: "task", row });
      // Same here: this session's pi went, its processes did not.
    }
    if (ev.type === "tool_execution_start") {
      // Only calls that CHANGE something count towards "this is work worth a plan": a read, a
      // search or a question is not (owner, 2026-09-17).
      const had = this.turnCalls.get(id);
      const changed = CHANGES.has(String(ev.toolName ?? "")) || String(ev.toolName ?? "").startsWith("kl_");
      this.turnCalls.set(id, { calls: (had?.calls ?? 0) + (changed ? 1 : 0), ...(had?.nudged ? { nudged: true as const } : {}) });
      // Work is starting and nothing is marked doing: the first thing waiting is what this is.
      this.plan(id, { type: "working" });
      const name = ev.toolName as string;
      // A call WAITING ON THE PERSON is not a background task: the question is already in the
      // composer, and the ledger showed it as "Lost · 1m 11s" because nothing but an answer would
      // ever end it (owner, 2026-09-17). Tasks are work that runs on its own.
      if (!WAITS.has(name)) {
        const row = this.write(() => this.tasks.transition({ id: ev.toolCallId as string, session: id, workspace: this.workspaceOf(id), tool: TOOL[name] ?? name, arg: argOf(name, (ev.args ?? {}) as Record<string, unknown>), state: "running", started: now }));
        if (row) this.emit({ type: "task", row });
      }
    }
    if (ev.type === "message_end") {
      const m = ev.message as { role?: string; content?: unknown } | undefined;
      if (m?.role === "assistant")
        this.lastSaid.set(id, typeof m.content === "string" ? m.content : (Array.isArray(m.content) ? m.content : []).map((c: { text?: string }) => c.text ?? "").join(""));
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
        if (ev.widgetKey === "harness:plan" && line) {
          // What this session says it is going to do. The panel draws it; nothing else reads it.
          const p = JSON.parse(line) as { set?: { text: string; state?: PlanState; why?: string }[]; done?: string; doing?: string; later?: { text: string; why?: string } };
          const items = p.done !== undefined
            ? this.plans.mark(id, p.done, "done")
            : p.doing !== undefined
              ? this.plans.mark(id, p.doing, "doing")
              : p.later !== undefined
                ? this.plans.mark(id, p.later.text, "later", p.later.why)
                : this.plans.set(id, p.set ?? []);
          this.write(() => items);
          this.emit({ type: "plan", session: id, items });
        }
        if (ev.widgetKey === "harness:proposal" && line) {
          // A tool asking to run: recorded here, drawn by the desktop, answered by a person.
          // `preview` is what the card shows under its line — a diff, the file, the command — so a
          // person asked to agree to an edit can read the edit (owner, 2026-09-18).
          const p = JSON.parse(line) as { id: string; tool: string; args: unknown; summary: string; preview?: string; question?: unknown };
          // Two children mint the SAME tool call id (`call_00_…`). Keyed by that alone, the second
          // session's card was dropped — `if (!has(p.id))` — and the surviving card carried the
          // FIRST session's id: the person's "yes" released the wrong tool call, and the session
          // that actually asked was told "declined by the person" while its workspace had been
          // created (api-test-report D3/D4). The key is the session AND the id; `raw` is what the
          // extension waits on, which is still its own id.
          // The id the DESKTOP answers by stays the child's own while only one session holds it —
          // the contract the routes and every window already use. A second session raising the same
          // id gets the scoped key, so both cards exist and each answer finds its own tool call.
          // A person reads this: ids become names where the bench knows one (R-D15).
          const named = this.knownNames();
          p.summary = withNames(p.summary, named);
          if (p.preview) p.preview = withNames(p.preview, named);
          if (p.question && typeof p.question === "object") {
            const q = p.question as { header?: string; options?: { label?: string; description?: string }[] };
            if (typeof q.header === "string") q.header = withNames(q.header, named);
            for (const o of q.options ?? []) {
              if (typeof o.label === "string") o.label = withNames(o.label, named);
              if (typeof o.description === "string") o.description = withNames(o.description, named);
            }
          }
          // ALWAYS scoped, never "scoped only when taken": a conditional key left the first card
          // on the bare id and the second on the scoped one, and the lookups then guessed between
          // them — an unrelated `yes` released someone else's tool call (R-D27).
          const key = proposalKey(id, p.id);
          if (!this.proposals.has(key)) this.proposals.set(key, { session: id, raw: p.id, tool: p.tool, summary: p.summary, preview: p.preview, args: p.args, question: p.question, wake: [] });
          this.emit({ type: "proposal", row: { id: key, session: id, tool: p.tool, args: p.args, summary: p.summary, preview: p.preview, question: p.question } });
          /**
           * A card raised while this session is working on somebody's ASK has nobody standing at
           * it: the person is watching the session that asked, not this one. `ask-1` sat running
           * for 240 s behind two write cards, and only moved when they were answered by hand
           * (api-test-report D6). So the asking session is told, and the card's own age counts
           * against the ask rather than looking like silence.
           */
          for (const a of this.asked.get(id) ?? []) {
            /**
             * The card is WHY this ask is quiet, so the ask remembers it: the idle clock counts the
             * wait, the asking session is told where to answer it, and if nobody ever does, the ask
             * ends saying so rather than sitting `running` for minutes (api-test-report D6, twice).
             */
            const clock = this.clocks.get(a.exchange) ?? { at: Date.now() };
            this.clocks.set(a.exchange, { ...clock, card: key });
            // The ask card is read by a PERSON: it says the workspace's name, and its summary has
            // already had every id resolved above (R-D15).
            const who = this.callIt(a.workspace, a.name);
            this.tell(a.from, `[${who} ${a.exchange}] ${who} is waiting for your approval — open it: ${withNames(p.summary, this.knownNames())}`);
            const row = this.write(() => this.exchanges.record({ id: `${a.exchange}-card-${Date.now().toString(36)}`, session: a.from, workspace: a.workspace, dir: "in", text: `waiting for approval: ${withNames(p.summary, this.knownNames())}`, state: "note", ref: a.exchange }));
            this.emit({ type: "exchange", row });
          }
        }
        if (ev.widgetKey === "harness:procs") {
          const ws = this.workspaceOf(id);
          const rows = (line ? JSON.parse(line) : []).map((r: Omit<ProcRow, "session">) => ({ ...r, workspace: ws })) as Omit<ProcRow, "session">[];
          this.write(() => this.procs.snapshot(id, rows));
          this.emit({ type: "procs", rows: this.procs.all() });
          // Something is running: from here the bench watches it, rather than waiting for the model
          // to call a tool again before anyone learns it stopped.
          if (rows.some((r) => r.ended === undefined)) this.pollProcs();
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

  /** One event, one plan. Writes and publishes only when something actually moved. */
  private plan(session: string, ev: PlanEvent) {
    const next = reduce(this.plans.get(session), ev);
    if (!next) return;
    this.write(() => this.plans.set(session, next));
    this.emit({ type: "plan", session, items: this.plans.get(session) });
  }

  private transitionAsk(a: { exchange: string; from: string; workspace: string }, state: string) {
    // Every state change restarts that state's own clock, and a settled exchange has none.
    if (state === "done" || state === "failed") this.clocks.delete(a.exchange);
    else this.clocks.set(a.exchange, { at: Date.now() });
    // Work handed to somebody else is in the plan too: the asker's, since that is who is waiting.
    if (state === "done") this.plan(a.from, { type: "answered", exchange: a.exchange });
    if (state === "failed") this.plan(a.from, { type: "ask_failed", exchange: a.exchange, task: a.workspace });
    this.write(() => this.exchanges.transition(a.exchange, state));
    this.emit({ type: "exchange", row: this.exchanges.bySession(a.from).find((e) => e.id === a.exchange) });
  }

  /**
   * A workspace session reporting on an ask it holds (spec §3.8). An ask is a small conversation,
   * not one reply: the first report is the DECISION ("going ahead with …"), relayed to the asking
   * session as a one-line update and settling nothing; the last is `done` or `blocked`, which
   * settles it with the shaped reply. The owner's own words: "it decides the change and tells the
   * main session it is going ahead with a specific change; then it builds, and once built and
   * pushed it informs the main agent that it is done."
   */
  async report(session: string, ask: string, kind: "progress" | "done" | "blocked", text: string): Promise<{ ask: string; kind: string; settled: boolean }> {
    const queue = this.asked.get(session) ?? [];
    const a = queue.find((x) => x.exchange === ask) ?? (queue.length === 1 ? queue[0] : undefined);
    if (!a) throw new Error(`no open ask ${ask} here`);
    // A report crosses to another session too: their note, not a paragraph about it (§3.8).
    const said = terse(String(text ?? ""));
    if (!said) throw new Error("a report needs something to say");
    if (kind === "progress") {
      // The ask stays running: a decision is not an answer, and the asking session waits for one.
      // Progress is part of the lifecycle (§3.9 rule 4): it resets the idle clock, so a workspace
      // that says what it is doing is never nudged or expired for going quiet.
      this.clocks.set(a.exchange, { at: Date.now() });
      // What a person reads in the Queue, so ids become names here too (R-D15).
      const row = this.exchanges.record({ id: `${a.exchange}-note-${Date.now().toString(36)}`, session: a.from, workspace: a.workspace, dir: "in", text: withNames(said, this.knownNames()).slice(0, 2000), state: "note", ref: a.exchange });
      this.write(() => row);
      this.emit({ type: "exchange", row });
      if (this.sessions.get(a.from)) await this.send(a.from, `[${this.callIt(a.workspace, a.name)} ${a.exchange}] ${brief(said, this.callIt(a.workspace, a.name))}`).catch(() => undefined);
      return { ask: a.exchange, kind, settled: false };
    }
    // `done` and `blocked` settle it exactly as a tagged reply does, through the one path that
    // pops the queue, records the answer and wakes the asking session.
    await this.deliver(session, [
      { role: "user", content: `[ask ${a.exchange} from ${a.from}] ${a.workspace}` },
      { role: "assistant", content: `[reply ${a.exchange}] ${kind === "blocked" ? `BLOCKED — ${said}` : said}` },
    ]);
    return { ask: a.exchange, kind, settled: true };
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
  /** A turn's own messages, as `agent_end` hands them over — the seam the contracts tests write to. */
  deliverForTest(id: string, answer: string): Promise<void> {
    return this.deliver(id, [{ role: "user", content: "[ask test] work" }, { role: "assistant", content: answer }]);
  }

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
    /**
     * WHICH assistant message is the answer. The last one is the ordinary case — but a `[task …
     * finished]` notice arriving mid-run starts another turn inside the SAME run, so the last
     * message was "Noted." and the `[reply …]` two messages above it settled nothing: the ask sat
     * `running` for a minute while the asking session polled six times and re-did the work
     * (owner, 2026-09-18). A tagged answer for an ask this session actually owes is the answer,
     * wherever in the run it was said.
     */
    const owed = (text: string) => {
      const tag = /\[reply ([^\]]+)\]/.exec(text)?.[1];
      return tag && queue.some((q) => q.exchange === tag) ? tag : undefined;
    };
    const tagged = all.map((m, i) => ({ i, role: m.role, text: said(m) })).filter((m) => m.role === "assistant" && owed(m.text));
    const end = tagged.length ? tagged[tagged.length - 1].i : all.map((m) => m.role).lastIndexOf("assistant");
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
    // An AGENT's session exists for one task and answers to one caller: every turn of it is that
    // answer, so it needs no tag. A workspace session is a conversation and does need one.
    const solo = queue.length === 1 && queue[0].agent;
    /**
     * A reply settles by INTENT, not by tag alone (spec §3.9 rule 3). A workspace turn that ends
     * while it holds exactly one open ask has answered that ask, whatever it remembered to write at
     * the front — the alternative is an exchange that waits out its deadline over a missing prefix.
     * The tag stays the fast path, and the session is told once how to say it.
     */
    const byIntent = !named && !asks.length && !solo && queue.length === 1 && !!answer;
    if (byIntent && !this.bounced.has(`tag:${queue[0].exchange}`)) {
      this.bounced.add(`tag:${queue[0].exchange}`);
      void this.send(id, `[harness] say \`[reply ${queue[0].exchange}]\` at the start of the answer so it reaches whoever asked`).catch(() => undefined);
    }
    if (!named && !asks.length && !solo && !byIntent) {
      // Nothing in this turn answered anybody — but a TAGGED reply for an ask already settled is an
      // update to it, not a turn that answered nobody: it is appended so the asking session sees it.
      if (replied) this.appendUpdate(replied, answer);
      return;
    }
    // The tag is the truth; FIFO is the fallback when a turn took an ask whose id it dropped, or
    // when several asks were merged into one run.
    const at = Math.max(0, queue.findIndex((x) => x.exchange === named));
    const [a] = queue.splice(at, 1);
    if (!queue.length) this.asked.delete(id);
    // What this reply changed about the architecture (§24). A reply that forgot to say is asked
    // ONCE — the work is done either way, and nagging twice would be its own conversation.
    const told = onReply(answer, this.bounced.has(a.exchange));
    if (told.rows.length) this.write(() => this.architecture.mergeContracts(told.rows));
    if (told.nudge) {
      // Asked once, and the work still settles: holding the answer back would make a missing line
      // into a stuck ask, which is worse than a line nobody wrote.
      this.bounced.add(a.exchange);
      void this.send(id, CONTRACTS_BOUNCE).catch(() => undefined);
    }
    this.transitionAsk(a, answer ? "done" : "failed");
    // A finished agent keeps its tree and its session: the person reads the transcript, opens the
    // diff, and closes it when the work is merged and clear (spec §4.1). Nothing is dropped on an
    // outcome, good or bad.
    const back = this.exchanges.record({ id: `${a.exchange}-in`, session: a.from, workspace: a.workspace, dir: "in", text: answer.slice(0, 2000), state: "done", ref: a.exchange });
    this.emit({ type: "exchange", row: back });
    // The asking session may be gone by now (removed, archived): an answer nobody is waiting for is dropped, not thrown.
    if (!answer || !this.sessions.get(a.from)) return;
    // The exchange row above keeps the whole reply; what crosses to the asking session is the
    // standup version of it, because that session is a planner and not a reader of diffs.
    // The tag a person reads carries the NAME; everything that routes carries the id.
    const who = a.name ?? a.workspace;
    await this.send(a.from, `[from ${a.agent ? "agent" : "workspace"} ${who}] ${brief(answer, who)}`, a.agent).catch(() => undefined);
  }

  /**
   * A process that ended on its own. Nothing tells the bench: the extension publishes the table
   * after a tool CALL, and a dev server that dies at 3am is between calls forever — the row stayed
   * "running", the panel lied, and the idle clock never let the bench sleep. So while any session
   * has a running row, its own tool server is asked every `PROC_POLL_MS`, and only then.
   */
  private pollProcs(): void {
    if (this.procPoll) return;
    this.procPoll = setInterval(() => {
      void this.sweepProcs().catch(() => undefined);
      void this.sweepWatches().catch(() => undefined);
    }, PROC_POLL_MS);
    this.procPoll.unref?.();
  }

  /** The 30 s beat that advances every deadline; started with the bench, stopped with it. */
  private sweepOn(): void {
    if (this.sweeper) return;
    this.sweeper = setInterval(() => void this.sweepExchanges().catch(() => undefined), SWEEP_MS);
    this.sweeper.unref?.();
  }

  /**
   * Deadlines (spec §3.9 rule 2). A `queued` ask the workspace never took is delivered once more
   * and then blocked; a `running` ask that has gone quiet is asked about once and then expired. The
   * asking session is told each time and the plan moves with it, because an exchange that changes
   * state without a row changing is the person kept in the dark (owner, 2026-09-18).
   *
   * `now` is a parameter so a table test can walk the clock instead of waiting ten minutes.
   */
  async sweepExchanges(now = Date.now()): Promise<void> {
    // A JOB with no end in sight: the session hears once, and the row stops claiming to be running.
    for (const t of this.tasks.all()) {
      if (t.state !== "background" && t.state !== "running") continue;
      if (now - t.started < JOB_MAX_MS) continue;
      const row = this.write(() => this.tasks.transition({ id: t.id, state: "lost", ended: now }));
      if (row) this.emit({ type: "task", row });
      this.tell(t.session, `[task ${t.arg} expired: it has been running for an hour with nothing to say]`);
    }
    for (const e of this.exchanges.active()) {
      const clock = this.clocks.get(e.id) ?? { at: e.ts };
      this.clocks.set(e.id, clock);
      const holder = [...this.asked.entries()].find(([, q]) => q.some((x) => x.exchange === e.id))?.[0];
      const age = now - clock.at;

      if (e.state === "queued") {
        if (age < ASK_PICKUP_MS) continue;
        // Nobody is holding it at all — the session went, or never opened. There is no second
        // delivery to make, and waiting again would be waiting on silence.
        if (!holder) {
          this.settle(e, "blocked", "the workspace session did not pick it up");
          continue;
        }
        // pi ALREADY HOLDS it when the session is mid-turn on an earlier ask: its queue is the
        // thing doing the waiting, and re-sending would hand the workspace the same work twice.
        if (this.turning.has(holder)) continue;
        if (!clock.redelivered) {
          // Once. A session that is simply busy gets its ask again; one that is gone does not
          // answer either way, and the next pass says so rather than waiting again.
          clock.redelivered = true;
          clock.at = now;
          // Never awaited: a session mid-turn answers its RPC when the turn ends, and a sweep that
          // waited for that would stop advancing every other deadline behind it.
          this.tell(holder, `[ask ${e.id} from ${e.session}] ${e.text}`);
          continue;
        }
        this.settle(e, "blocked", "the workspace session did not pick it up");
        continue;
      }

      // Running: quiet for too long is asked about once, then given up on.
      if (clock.nudgedAt === undefined) {
        if (age < ASK_IDLE_MS) continue;
        clock.nudgedAt = now;
        if (holder) this.tell(holder, `[harness] still on ${e.id}? report progress, done or blocked`);
        this.emit({ type: "exchange", row: { ...e, state: "running" } });
        continue;
      }
      if (now - clock.nudgedAt < ASK_NUDGE_GRACE_MS) continue;
      // A card nobody answered is not a workspace that went quiet: say which it was, and take the
      // card away, so the workspace is not left blocked on a question with no asker.
      if (clock.card) {
        const card = clock.card;
        this.settle(e, "blocked", "waiting for an approval nobody gave");
        if (!this.proposals.get(card)?.answer) this.answerProposal(card, "no");
        continue;
      }
      this.settle(e, "expired", "the workspace went quiet");
    }
  }

  /**
   * A reply for an ask that is already settled: appended to it as an update, never a new exchange
   * and never a second answer (spec §3.9 rule 3). A workspace that finishes late still has
   * something worth saying, and dropping it silently is the same stale stop from the other end.
   */
  private appendUpdate(exchange: string, text: string): void {
    const of = this.exchanges.get(exchange);
    if (!of || !text.trim()) return;
    const row = this.write(() => this.exchanges.record({ id: `${exchange}-late-${Date.now().toString(36)}`, session: of.session, workspace: of.workspace, dir: "in", text: text.slice(0, 2000), state: "note", ref: exchange }));
    this.emit({ type: "exchange", row });
    if (this.sessions.get(of.session)) void this.send(of.session, `[${of.workspace} ${exchange}] ${brief(text, of.workspace)}`).catch(() => undefined);
  }

  /**
   * A line to a session, for the SWEEP: it never waits on the answer, and never starts a child that
   * is not already there. An awaited `send` to a session mid-turn resolves only when that turn
   * ends, which stalls every other deadline behind it — and in a test it stalls the process.
   */
  private tell(session: string, line: string): void {
    if (!this.children.has(session) || !this.sessions.get(session)) return;
    void this.send(session, line).catch(() => undefined);
  }

  /** One ended exchange: the row, the plan and the asking session, in that order and always together. */
  private settle(e: { id: string; session: string; workspace: string }, state: string, why: string): void {
    this.clocks.delete(e.id);
    for (const [id, q] of this.asked) {
      const left = q.filter((x) => x.exchange !== e.id);
      if (left.length !== q.length) (left.length ? this.asked.set(id, left) : this.asked.delete(id));
    }
    this.write(() => this.exchanges.transition(e.id, state));
    this.plan(e.session, { type: "ask_failed", exchange: e.id, task: e.workspace });
    this.emit({ type: "exchange", row: this.exchanges.bySession(e.session).find((x) => x.id === e.id) });
    this.tell(e.session, `[${e.workspace} ${e.id}] ${state}: ${why}`);
  }

  private async sweepProcs(): Promise<void> {
    // Every session with a row, running or not: a row marked lost is exactly the one that needs
    // asking about, and only its own tool server can say.
    const sessions = [...new Set(this.procs.all().map((p) => p.session))];
    if (!sessions.length && !this.watching.size) {
      clearInterval(this.procPoll);
      this.procPoll = undefined;
      return;
    }
    for (const session of sessions) {
      const live = await this.listProcs(session).catch(() => undefined);
      if (!live) {
        // A tool server that cannot be asked is not an answer — but a row nobody can ever verify
        // would hold the bench awake forever, so after `UNREACHABLE_SWEEPS` it is lost, which is
        // exactly what it is: gone, with nobody able to say how it ended.
        const n = (this.unreachable.get(session) ?? 0) + 1;
        this.unreachable.set(session, n);
        if (n < UNREACHABLE_SWEEPS) continue;
        this.unreachable.delete(session);
        let gone = false;
        for (const p of this.procs.all().filter((x) => x.session === session && x.ended === undefined)) {
          gone = !!this.write(() => this.procs.transitionEnded(session, p.id, null, true)) || gone;
          // Told ONCE, with what it last printed. A row that quietly turns to "lost" is a process
          // the session goes on believing in (spec §3.9 rule 8).
          void this.notifyLost(session, p.id, p.name).catch(() => undefined);
        }
        if (gone) this.emit({ type: "procs", rows: this.procs.all() });
        continue;
      }
      this.unreachable.delete(session);
      let moved = false;
      // Alive after all: the tool server is running it, whatever this ledger said.
      for (const p of this.procs.all().filter((x) => x.session === session && (x.ended !== undefined || x.lost))) {
        if (live.some((l) => l.id === p.id && l.state !== "exited")) moved = !!this.write(() => this.procs.revive(session, p.id)) || moved;
      }
      for (const p of this.procs.all().filter((x) => x.session === session && x.ended === undefined)) {
        const now = live.find((x) => x.id === p.id);
        // Gone from the tool server's list, or exited in it: either way it is over.
        if (now && now.state !== "exited") continue;
        // Gone from the tool server's own list without ever reporting an exit: THAT is lost.
        const row = this.write(() => this.procs.transitionEnded(session, p.id, now?.exit_code ?? null, !now));
        moved = !!row || moved;
        // Nobody polls: the session is TOLD, with enough of the output to know what happened.
        if (row && now) void this.notifyEnded(session, row.id, row.name, now.exit_code ?? null).catch(() => undefined);
      }
      if (moved) this.emit({ type: "procs", rows: this.procs.all() });
    }
  }

  private async listProcs(session: string): Promise<{ id: string; state?: string; exit_code?: number | null }[]> {
    const r = await this.toolsFetch(session, "/tools/process_list", { method: "POST", headers: { "content-type": "application/json" }, body: "{}" });
    if (!r.ok) throw new Error(`process_list: ${r.status}`);
    return ((await r.json()) as { processes?: { id: string; state?: string; exit_code?: number | null }[] }).processes ?? [];
  }

  /**
   * A background command finished. The model does not watch it — it is told, in its own turn order,
   * with the tail of what it printed, because "it failed" without the last twenty lines is a
   * message that only makes somebody go and look.
   */
  private async notifyEnded(session: string, id: string, title: string, code: number | null) {
    const out = await this.procOutput(id, 0).catch(() => undefined);
    const tail = [out?.stdout, out?.stderr].filter(Boolean).join("\n").replace(/\s+$/, "").split("\n").slice(-20).join("\n");
    await this.send(session, `[task ${title} finished: exit ${code ?? "?"}]${tail ? `\n${tail}` : ""}`).catch(() => undefined);
  }

  /**
   * A process nobody can find any more: the pod restarted, or the workspace is gone. The session
   * that started it is told once, with the last lines it managed to read — never left with a row
   * that says "running" about something that is not (spec §3.9 rule 8).
   */
  private async notifyLost(session: string, id: string, title: string): Promise<void> {
    // Whatever it managed to print — but its tool server is exactly the one that could not be
    // asked, so this is best effort and never waited on for long.
    const out = await this.procOutput(id, 0).catch(() => undefined);
    const tail = [out?.stdout, out?.stderr].filter(Boolean).join("\n").replace(/\s+$/, "").split("\n").slice(-20).join("\n");
    this.tell(session, `[task ${title} lost: nobody can say how it ended]${tail ? `\n${tail}` : ""}`);
  }

  /**
   * Another WORKSPACE is what a session is waiting on, and a workspace can stop, idle, be deleted
   * or lose its node. Every open exchange on it ends rather than waiting out a deadline that says
   * nothing about why (spec §3.9 rule 8). The same call settles an agent's exchanges when the tree
   * it worked in is deleted — a tree that is gone is a machine that is gone.
   */
  workspaceGone(workspace: string, why = "workspace stopped"): void {
    for (const e of this.exchanges.active()) {
      if (e.workspace !== workspace) continue;
      this.settle(e, "blocked", why);
    }
    /**
     * Its PROCESSES went with it. Stopping a workspace with a live detached process left the row
     * saying `running` and told nobody, so the session went on believing in a dev server that had
     * been gone for minutes (api-test-report R-D25, spec §3.9 rule 8). Lost, said once, and the row
     * says so.
     */
    for (const p of this.procs.all().filter((x) => x.workspace === workspace && x.ended === undefined)) {
      const row = this.write(() => this.procs.transitionEnded(p.session, p.id, null, true));
      if (row) this.emit({ type: "procs", rows: this.procs.all() });
      this.tell(p.session, `[task ${p.name} lost: ${why}]`);
    }
    // A watch on a process in that workspace has nothing left to watch.
    for (const [id, w] of this.watching) {
      const row = this.procs.all().find((p) => p.id === id);
      if (row?.workspace === workspace) {
        this.watching.delete(id);
        this.tell(w.session, `[watch ${row.name} ended: ${why}]`);
      }
    }
  }

  /**
   * A watch: lines of a running process that match a pattern, delivered as they appear. One message
   * per batch (debounced), never one per line — a `watch` on a noisy log would otherwise be a way
   * to fill a context window from a dev server.
   */
  watchProc(session: string, id: string, pattern: string): void {
    const re = new RegExp(pattern);
    this.watching.set(id, { session, re, since: 0, sinceErr: 0, pattern, said: new Set(), fires: 0 });
    this.pollProcs();
  }

  private async sweepWatches(): Promise<void> {
    for (const [id, w] of [...this.watching]) {
      const row = this.procs.all().find((p) => p.id === id);
      if (!row || row.ended !== undefined) {
        this.watching.delete(id);
        continue;
      }
      const out = await this.procOutput(id, w.since, w.sinceErr).catch(() => undefined);
      if (!out) continue;
      // BOTH cursors. stderr used to be read from 0 on every poll (`crates/ide/src/tools/exec.rs`
      // before 81621d02), so a build — whose progress is all on stderr — replayed the same
      // `#N DONE` lines on every fire (owner, 2026-09-18). The dedupe below stays as belt and
      // braces: a process that reprints a line is not the same thing as a cursor that never moved.
      w.since = out.next ?? w.since;
      w.sinceErr = out.next_err ?? w.sinceErr;
      const hit = [out.stdout, out.stderr]
        .filter(Boolean)
        .join("\n")
        .split("\n")
        .filter((l) => l.trim() && w.re.test(l) && !w.said.has(l))
        .slice(0, WATCH_MAX_LINES);
      for (const l of hit) w.said.add(l);
      // A build prints thousands of lines; only what matched is remembered, and never unboundedly.
      if (w.said.size > 2_000) w.said = new Set([...w.said].slice(-1_000));
      if (!hit.length) continue;
      // A watch is BOUNDED (spec §3.9 rule 5): twenty batches is a standing subscription, not a
      // notification, and the last one says so rather than going quiet on its own.
      w.fires += 1;
      const last = w.fires >= WATCH_MAX_FIRES;
      if (last) this.watching.delete(id);
      await this.send(w.session, `[watch ${row.name} /${w.pattern}/]\n${hit.join("\n")}${last ? "\n[watch ended: it has fired 20 times; watch it again if you still need it]" : ""}`).catch(() => undefined);
    }
  }

  /**
   * A process's output, from the tool server that is running it: the desktop's log view reads this.
   * The two streams have their own offsets (`since`/`next`, `since_err`/`next_err`, 81621d02), and a
   * reader that keeps only one re-reads the other from the start for as long as the process lives.
   */
  async procOutput(
    id: string,
    since: number,
    sinceErr = 0,
  ): Promise<{ stdout: string; stderr: string; next: number; next_err?: number; state?: string; exit_code?: number | null }> {
    const row = this.procs.all().find((p) => p.id === id);
    if (!row) throw new Error(`no process ${id}`);
    const r = await this.toolsFetch(row.session, "/tools/process_output", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ id, since, since_err: sinceErr }) });
    if (!r.ok) throw new Error(`process ${id}: the tool server answered ${r.status}`);
    return (await r.json()) as { stdout: string; stderr: string; next: number; next_err?: number };
  }

  /**
   * Which WORKSPACE a session's processes and tasks belong to (owner, 2026-09-17). Every bench
   * session shares the bench's own machine; a workspace session — and the agents working in it —
   * share that workspace, an agent included — it works in a TREE of it, not a second workspace, so
   * its processes and tasks are that workspace's. It is never the session id: two bench sessions must
   * see the same dev server.
   */
  workspaceOf(id: string): string {
    const s = this.sessions.get(id);
    return s?.target || s?.workspace || process.env.KL_WORKSPACE_ID || "bench";
  }

  /**
   * Where a session's tools run: the workspace it names. A BENCH session names none — it has no
   * tool server anywhere (spec §3.1) — so anything that needed one is a question about a session
   * that never had hands, answered as that rather than dialled at the bench's own pod.
   */
  private async toolsAddress(session: string): Promise<string> {
    return (await this.toolsFor(session)).address;
  }

  /**
   * Where a session's tool server is AND the token it requires. Every `/tools/*`, `/fs/*` and
   * `/stream/*` call the bench makes goes through here, so the header is sent in one place rather
   * than at five call sites that each forgot it. The token is never logged, never put in a tool
   * result, and never sent to the renderer.
   */
  private async toolsFor(session: string, fresh = false): Promise<{ address: string; token?: string }> {
    const s = this.sessions.get(session);
    if (!s) throw new Error(`no session ${session}`);
    if (!s.target) throw new Error("this session has no machine; a process runs in a workspace");
    return this.toolsOf(s.target, fresh);
  }

  /** The same, by workspace id: the tree-readiness wait has no session to ask with. */
  private async toolsOf(workspace: string, fresh = false): Promise<{ address: string; token?: string }> {
    const mod = await import("../../pi/workspace-tools.ts");
    // An injected resolver (tests, and a bench pointed at a fixed address) answers an address only.
    if (this.opts.resolveTools) {
      const address = await this.opts.resolveTools(workspace);
      const token = await mod.toolsAuth(workspace, fresh).then((a) => a.token, () => undefined);
      return { address, ...(token ? { token } : {}) };
    }
    return mod.toolsAuth(workspace, fresh);
  }

  /**
   * One call to a tool server, with the workspace's token. A 401 is a STALE token — the keys beat
   * re-mints it — so it resolves once more and tries again; a second 401 is a plain error.
   */
  private async toolsFetch(session: string, path: string, init: RequestInit & { headers?: Record<string, string> } = {}): Promise<Response> {
    for (let attempt = 0; ; attempt++) {
      const at = await this.toolsFor(session, attempt > 0);
      const headers = { ...(init.headers ?? {}), ...(at.token ? { authorization: `Bearer ${at.token}` } : {}) };
      const r = await fetch(`http://${at.address}${path}`, { ...init, headers });
      if (r.status !== 401 || attempt > 0) return r;
      await r.body?.cancel().catch(() => undefined);
      const mod = await import("../../pi/workspace-tools.ts");
      const s = this.sessions.get(session);
      if (s?.target) mod.forgetToolsAuth(s.target);
    }
  }

  /**
   * Stop a background process. It runs on a tool server — the session's own workspace — so this is
   * the harness reaching the same place the tool did, never a command typed at the model.
   */
  async killProc(session: string, id: string): Promise<void> {
    const r = await this.toolsFetch(session, "/tools/process_kill", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ id }) });
    if (!r.ok) throw new Error(`process ${id}: the tool server answered ${r.status}`);
    const row = this.write(() => this.procs.transitionEnded(session, id));
    if (row) this.emit({ type: "procs", rows: this.procs.all() });
  }

  /**
   * A task cancelled by a PERSON, from the desktop. It is recorded here and never spoken to the
   * model: a `/cancel #3` sent as a prompt landed in pi's context and in its session file, so every
   * reopen replayed it as something the person had said (owner: "don't spoil the session with this
   * data"). A task pi is running inside its own turn is still cancelled through the extension
   * command in `remove()` — that one is pi's to act on.
   */
  cancelTask(id: string): { id: string; state: string } {
    const t = this.tasks.all().find((x) => x.id === id);
    if (!t) throw new Error(`no task ${id}`);
    if (t.state === "running" || t.state === "background") {
      const row = this.write(() => this.tasks.transition({ id, state: "cancelled", ended: Date.now() }));
      if (row) this.emit({ type: "task", row });
    }
    return { id, state: "cancelled" };
  }

  /**
   * A turn ended with the plan out of date — an item still `doing`, or real work done with no plan
   * at all. The harness says so ONCE, as a follow-up the model answers with a `plan` call: the
   * model forgetting is the common case, and a panel that lies is worse than a line of nagging.
   */
  /**
   * The conversation grew past what the model can hold. pi summarises and continues (`compact`),
   * and the instruction says what must survive: the plan, what is still outstanding, and what the
   * person has told us — everything else is recoverable, those three are not.
   */
  private async compactIfFull(id: string, used: number, window?: number) {
    const full = window ? used / window >= COMPACT_AT : used >= COMPACT_TOKENS;
    if (!full || this.compacting.has(id)) return;
    this.compacting.add(id);
    const plan = this.plans.get(id).filter((x) => x.state !== "done");
    const open = (this.asked.get(id) ?? []).map((a) => a.workspace);
    const keep = [
      "Keep, verbatim where you can:",
      plan.length ? `the plan, with state: ${plan.map((x) => `${x.text.split("\u0000")[0]} (${x.state})`).join("; ")}` : "there is no plan",
      open.length ? `what is still outstanding: ${open.join(", ")}` : "nothing is outstanding",
      "what the person has told you about themselves and their setup, and what they asked for that is not done yet.",
      "Drop tool output, file contents and anything already reported.",
    ].join("\n");
    try {
      await this.children.get(id)?.send({ type: "compact", customInstructions: keep });
      this.emit({ type: "compacted", session: id, at: Date.now() });
    } catch {
      /* a compaction that failed is a turn that carries on: it is a tidy-up, not a gate */
    } finally {
      this.compacting.delete(id);
    }
  }

  private keepPlanCurrent(id: string) {
    const turn = this.turnCalls.get(id);
    if (!turn || turn.nudged) return;
    // Something it is waiting on is not something it forgot: an outstanding ask keeps its item doing.
    const waiting = (this.asked.get(id)?.length ?? 0) > 0;
    const line = waiting ? undefined : nudge(this.plans.get(id), turn.calls, this.lastSaid.get(id) ?? "");
    if (!line) return;
    this.turnCalls.set(id, { ...turn, nudged: true });
    void this.send(id, line).catch(() => undefined);
  }

  /** A prompt into a session that may be mid-turn: pi queues a follow-up rather than refusing. */
  private send(id: string, message: string, direct = false): Promise<PiEvent> {
    /**
     * `turning` only becomes true when `agent_start` comes BACK from the child, and that is a round
     * trip later than the prompt that caused it. Three asks half a second apart therefore raced:
     * the first was sent as a `prompt` and the second arrived while the answer to it was still in
     * flight, so it was sent as a prompt too — and pi, already starting a turn, kept the last one.
     * Ask #1 never ran (api-test-report R-D24). A prompt this session has SENT but not yet seen
     * start counts as mid-turn, which is what it is.
     */
    const mid = this.turning.has(id) || this.starting.has(id);
    // A session and its OWN agents talk directly (owner, 2026-09-17): a dispatch goes at once, and
    // a report comes back at once — as a steer when the caller is mid-turn, so it is read in this
    // turn rather than behind whatever else is queued. The fork-ordered inbox is for prompts from
    // OTHER sessions and from people, which is where ordering is a judgement at all.
    if (direct) return this.rpc(id, { type: mid ? "steer" : "prompt", message });
    // Held from here until the turn this prompt starts is seen, so the next one queues behind it.
    if (!mid) this.starting.add(id);
    const r = this.rpc(id, { type: mid ? "follow_up" : "prompt", message }).finally(() => {
      if (mid) return;
      // The turn either started (and `turning` now holds it) or the send failed; either way this
      // seat is free.
      if (this.turning.has(id)) this.starting.delete(id);
      else setTimeout(() => this.starting.delete(id), START_GRACE_MS).unref?.();
    });
    if (mid) this.triageSoon(id);
    return r;
  }

  /**
   * Order the queue of a session that is mid-turn, by asking a FORK of it: it has the context to
   * know which reply unblocks the work and which question can wait, and it costs the running turn
   * nothing. Debounced, because a burst of arrivals is one ordering.
   *
   * Steering is never touched: a steer is "change what you are doing NOW", and re-ordering that
   * would be reordering an interruption.
   */
  private triageSoon(id: string, delayMs = TRIAGE_DEBOUNCE_MS) {
    if (this.triaging.has(id) || this.opts.readOnly) return;
    const t = setTimeout(() => {
      this.triaging.delete(id);
      void this.triageNow(id).catch(() => undefined);
    }, delayMs);
    t.unref?.();
    this.triaging.set(id, t);
  }

  async triageNow(id: string): Promise<{ order: number[]; reasons: (string | undefined)[] } | undefined> {
    const c = this.children.get(id);
    if (!c?.running()) return undefined;
    const taken = (await c.send({ type: "clear_queue" })).data as { steering?: string[]; followUp?: string[] } | undefined;
    const steering = taken?.steering ?? [];
    const items = taken?.followUp ?? [];
    // Put steering straight back: it is an interruption, and order does not apply to it.
    for (const m of steering) await c.send({ type: "steer", message: m }).catch(() => undefined);
    if (items.length < 2) {
      for (const m of items) await c.send({ type: "follow_up", message: m }).catch(() => undefined);
      return undefined;
    }
    let ranked: { index: number; reason?: string }[] | undefined;
    try {
      const answer = await this.btw(id, triageQuestion(items), TRIAGE_TIMEOUT_MS);
      const said = (answer.entries as { role?: string; content?: unknown }[]).filter((m) => m.role === "assistant");
      ranked = order(said.map((m) => (typeof m.content === "string" ? m.content : ((m.content as { text?: string }[]) ?? []).map((x) => x.text ?? "").join(""))).join("\n"), items.length);
    } catch {
      /* a fork that failed orders nothing */
    }
    // Whatever went wrong, every message goes back in: the only rule is never to lose one.
    // And the person's own prompts keep the order they were typed in, whatever the fork decided.
    const rows = keepPersonOrder(ranked ?? items.map((_, index) => ({ index })), items);
    for (const r of rows) await c.send({ type: "follow_up", message: items[r.index] }).catch(() => undefined);
    this.emit({ type: "queue_order", session: id, items: rows.map((r) => ({ text: items[r.index], reason: r.reason })) });
    return { order: rows.map((r) => r.index), reasons: rows.map((r) => r.reason) };
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
   * Every state of it has a deadline (§3.9 rule 2), including the one where it is waiting on a card
   * nobody is standing at (rule 2, D6).
   */
  async ask(to: string, text: string, from: string): Promise<{ session: string; exchange: string; workspace: string; queued: number }> {
    this.refuse(true);
    if (typeof text !== "string" || !text.trim()) throw new Error("an ask needs something to do");
    const asker = this.sessions.get(from);
    if (!asker) throw new Error(`no session ${from}`);
    // A LIVE AGENT by name is resumed, not replaced: its context is the whole reason to send it more.
    const agent = this.sessions.get(`e-${to}`);
    // Everything downstream — the session key, the fork's file, `KL_TOOLS_WORKSPACE` — is the ID.
    // The name survives only in what a person reads.
    const { id: workspace, name } = agent && !agent.archived ? { id: to, name: agent.name || to } : await this.resolveWorkspace(to);
    const s = agent && !agent.archived ? agent : await this.openWorkspace(workspace);
    const direct = !!agent && !agent.archived;
    // The SAME ask, again, while the first is still open: one exchange, not two (spec §3.9 rule 6).
    // A bench that re-asks after a poll would otherwise have the workspace do the work twice.
    const open = this.exchanges.active().find((e) => e.session === from && e.workspace === workspace && e.text.trim() === text.trim());
    if (open) {
      void this.send(from, `[harness] already asked: ${open.id}, still ${open.state}`).catch(() => undefined);
      return { session: s.id, exchange: open.id, workspace, queued: (this.asked.get(s.id) ?? []).length };
    }
    const exchange = `ask-${++this.askSeq}-${Date.now().toString(36)}`;
    const row = this.write(() => this.exchanges.record({ id: exchange, session: from, workspace, dir: "out", text, state: "queued" }));
    this.emit({ type: "exchange", row });
    const queue = this.asked.get(s.id) ?? [];
    queue.push({ exchange, from, workspace, name });
    this.asked.set(s.id, queue);
    this.plan(from, { type: "asked", exchange, to: workspace, task: text });
    try {
      // A live agent of this session is talked to directly; a workspace is a teammate with a queue.
      // The person's words as they stand — but a model asking on their behalf writes "Sure! I'll
      // ask the workspace to …" in front of them, and that is not the person's words (§3.8).
      const said = terse(text);
      await this.send(s.id, direct ? said : `[ask ${exchange} from ${asker.name}] ${said}`, direct);
    } catch (e) {
      this.asked.set(s.id, queue.filter((x) => x.exchange !== exchange));
      this.transitionAsk({ exchange, from, workspace }, "failed");
      throw e;
    }
    return { session: s.id, exchange, workspace, queued: queue.length };
  }

  /**
   * The extension's side of a proposal: wait until a person answers, or until the cap. An unanswered
   * question is a NO — the whole point is that nothing changes without somebody saying yes.
   */
  waitProposal(id: string, capMs: number, signal?: AbortSignal, session?: string): Promise<string> {
    // The extension waits on the id IT minted and says which session it is; the desktop answers by
    // the key it was handed. There is NO search by raw id: with the same id open in two sessions
    // that can only guess, and a guess here releases the wrong tool call.
    // The extension waits on the id its child minted and names its session; the desktop waits on
    // the key it was handed, which already identifies the card. Either addresses ONE card — there
    // is no search by raw id, because with the same id open twice that can only guess.
    const key = session && !this.proposals.has(id) ? proposalKey(session, id) : id;
    const p = this.proposals.get(key);
    if (!p) return Promise.resolve("no");
    if (p.answer) return Promise.resolve(p.answer);
    return new Promise((resolve) => {
      const done = (a: string) => {
        clearTimeout(timer);
        this.proposals.get(key)?.wake.splice(0);
        resolve(a);
      };
      // The cap answers only a card nobody answered: a person who got there first has already
      // settled it, and `answerProposal` now refuses a second answer — thrown from a timer, that
      // would take the process down.
      const timer = setTimeout(() => {
        try {
          if (!this.proposals.get(key)?.answer) this.answerProposal(key, "no");
        } catch {
          /* answered in the same tick: the first answer stands */
        }
        done("no");
      }, capMs);
      timer.unref?.();
      p.wake.push(() => done(this.proposals.get(key)?.answer ?? "no"));
      // A client that went away takes its question with it; the tool call is over either way.
      signal?.addEventListener("abort", () => done("no"), { once: true });
    });
  }

  /**
   * A person's answer. The first stands; a SECOND is a conflict, not a second answer — answering
   * twice returned 200 both times, which reads as though the later answer took (D12).
   */
  answerProposal(id: string, answer: string): { id: string; answer: string } {
    // One card, addressed by its own key. A raw tool-call id names nothing: it can be open in two
    // sessions at once, and answering "whichever" is what declined the person's own card (R-D27).
    const key = id;
    const p = this.proposals.get(key);
    if (!p) throw new Error(`no proposal ${id}`);
    if (p.answer !== undefined) throw new AlreadyAnswered(id, p.answer);
    p.answer = answer;
    // The ask that was waiting on this card is waiting on work again: its idle clock restarts from
    // the answer, not from whenever the card went up (D6).
    for (const [exchange, c] of this.clocks) if (c.card === key) this.clocks.set(exchange, { at: Date.now() });
    // A no means the item the turn was on is not happening: the plan says so, with the reason.
    if (p.answer === "no") this.plan(p.session, { type: "declined" });
    // The answer reaches the model as the question tool's RESULT — `waitProposal` is what the tool
    // call is awaiting, and it resolves with exactly this. It used to ALSO be sent as a prompt, so
    // the model was told twice and pi wrote a `yes` user message into the session file, which every
    // reopen then replayed under the card that already said it (owner, on the transcript).
    this.emit({ type: "proposal", row: { id: key, session: p.session, tool: p.tool, args: p.args, summary: p.summary, answer: p.answer } });
    for (const w of p.wake.splice(0)) w();
    return { id: key, answer: p.answer };
  }

  /** What is still being asked, for a window that opened after the question did. */
  openProposals(): { id: string; session: string; tool: string; args: unknown; summary: string; question?: unknown }[] {
    return [...this.proposals.entries()].filter(([, p]) => !p.answer).map(([id, p]) => ({ id, session: p.session, tool: p.tool, args: p.args, summary: p.summary, question: p.question }));
  }

  /**
   * An AGENT: a fresh session in a workspace, given one task and no history, whose answer comes
   * back to whoever started it. It is an ask (§2's machinery, unchanged) to a session that did not
   * exist a moment ago — which is the whole difference between an agent and a teammate: the
   * teammate remembers, the agent starts clean and is thrown away.
   *
   * Several run at once because each has its own session; the caller carries on meanwhile.
   */
  async agent(to: string, task: string, name: string, from: string, model?: string): Promise<{ session: string; exchange: string; name: string; tree: string }> {
    this.refuse(true);
    if (typeof task !== "string" || !task.trim()) throw new Error("an agent needs a task");
    const caller = this.sessions.get(from);
    if (!caller) throw new Error(`no session ${from}`);
    /**
     * An agent works in the CALLER's workspace, full stop. Asked for an agent in an empty
     * workspace, the bench cut its tree in the person's own `backend` instead and briefed it to
     * write there — hands in a workspace nobody named (api-test-report R-N1). The binding is here,
     * not in the wording of a prompt: a workspace session's agent is bound to that session's own
     * workspace whatever the model asked for, and a bench session — which has no workspace of its
     * own — must name one and gets exactly the one it named.
     */
    if (caller.workspace) {
      if (to && to.trim() && to.trim() !== caller.workspace && to.trim() !== caller.name) {
        throw new Error(`an agent works in this workspace (${caller.name || caller.workspace}), not ${to.trim()}; ask that workspace instead`);
      }
      to = caller.workspace;
    } else if (!to || !to.trim()) {
      throw new Error("which workspace should the agent work in? name one");
    }
    // The workspace it works in is an ID before any child exists — an agent bound to a name has no
    // hands (owner, 2026-09-17).
    const { id: workspace } = await this.resolveWorkspace(to);
    // Its own TREE of that workspace, not a second workspace: a nested snapshot inside the same
    // pod, cut by the node agent on this ask, with the caches already warm (spec §4.1).
    const tree = await this.cutTree(workspace, name);
    const s = await this.openEphemeral(workspace, name, tree);
    this.trees.set(name, { workspace, tree, from });
    if (model && this.sessions.get(s.id)?.model !== model) this.write(() => this.sessions.update(s.id, { model }));
    const exchange = `agent-${++this.askSeq}-${Date.now().toString(36)}`;
    const row = this.write(() => this.exchanges.record({ id: exchange, session: from, workspace, dir: "out", text: task, state: "queued" }));
    this.emit({ type: "exchange", row });
    const queue = this.asked.get(s.id) ?? [];
    queue.push({ exchange, from, workspace: name, agent: true });
    this.asked.set(s.id, queue);
    this.plan(from, { type: "asked", exchange, to: name, task });
    try {
      // No tag and no history: an agent is given the task and nothing else, so its answer is the
      // answer to that task rather than to a conversation it was not part of. Direct: an agent is
      // this session's own, not a queue it has to wait in.
      await this.send(s.id, terse(task), true);
    } catch (e) {
      this.asked.set(s.id, queue.filter((x) => x.exchange !== exchange));
      this.transitionAsk({ exchange, from, workspace: name }, "failed");
      throw e;
    }
    return { session: s.id, exchange, name, tree };
  }

  /**
   * Stop an agent that is still working, before its session goes. `remove()` cancels its tasks and
   * kills the child, but a turn mid-flight would be killed rather than ended — an abort lets it
   * stop its own tool calls first, and the wait is capped because a child that will not stop is
   * being killed anyway (§17.9).
   */
  async abortAgent(name: string, capMs = ABORT_WAIT_MS): Promise<void> {
    const id = `e-${name}`;
    const c = this.children.get(id);
    if (!c?.running() || !this.turning.has(id)) return;
    const ended = new Promise<void>((resolve) => {
      const off = this.onEvent((ev) => {
        if (ev.pi === id && (ev.type === "agent_end" || ev.type === "exit")) (off(), clearTimeout(t), resolve());
      });
      const t = setTimeout(() => (off(), resolve()), capMs);
      t.unref?.();
    });
    await c.send({ type: "abort" }).catch(() => undefined);
    await ended;
  }

  /** The tree each live agent works in, by agent name, so a close takes it back. */
  private trees = new Map<string, { workspace: string; tree: string; from: string }>();
  /** The tree an agent is working in, if any: the close deletes it and the sidebar labels it. */
  treeOf(name: string): { workspace: string; tree: string } | undefined {
    const t = this.trees.get(name);
    return t && { workspace: t.workspace, tree: t.tree };
  }
  /** Agents whose caller is this session: removing a session closes what it started. */
  agentsOf(session: string): string[] {
    return [...this.trees.entries()].filter(([, v]) => v.from === session).map(([name]) => name);
  }
  forgetTree(name: string): void {
    this.trees.delete(name);
  }

  /**
   * `/v1`: the one platform call the bench makes for itself. Everything else a session does to the
   * platform is a `kl_*` tool in a child; a tree is not, because the session that will work in it
   * does not exist until it is ready.
   */
  private v1(method: string, p: string, body?: unknown): Promise<{ status: number; data: unknown }> {
    const call = this.opts.platform ?? ((m: string, q: string, b?: unknown) => import("../../pi/kloudlite.ts").then((x) => x.call(m, q, b)));
    return call(method, p, body);
  }

  /**
   * Cut the agent's tree and wait for it to be usable. `/v1` writes spec and answers 202; what
   * matters here is not the CR but whether the tools work in it, so the wait asks the workspace's
   * own tool server — which serves a tree only once the subvolume exists (`Trees::resolve`). That
   * is the same fact `status.trees[name].ready` carries, observed where it is about to be used.
   */
  private async cutTree(workspace: string, name: string): Promise<string> {
    const r = await this.v1("POST", `/v1/workspaces/${encodeURIComponent(workspace)}/trees`, { name });
    if (r.status >= 400) throw new Error(typeof r.data === "string" ? r.data : `the platform would not cut a tree (${r.status})`);
    const until = Date.now() + TREE_READY_MS;
    for (;;) {
      if (await this.treeServed(workspace, name)) return name;
      if (Date.now() > until) throw new Error(`the workspace did not make a working directory for ${name} in time`);
      await new Promise((ok) => setTimeout(ok, TREE_POLL_MS).unref?.());
    }
  }

  /** Whether the workspace's tool server serves this tree yet: a read of its root, nothing more. */
  private async treeServed(workspace: string, tree: string): Promise<boolean> {
    try {
      const at = await this.toolsOf(workspace);
      const r = await fetch(`http://${at.address}/fs/stat?tree=${encodeURIComponent(tree)}&path=.`, { headers: at.token ? { authorization: `Bearer ${at.token}` } : {} });
      return r.ok;
    } catch {
      // Not answering yet is not a refusal: the pod may still be coming up, and the cap is the bound.
      return false;
    }
  }

  /** The tree the agent's ports live in, said once, at the top of its brief (spec §4.6). */
  private treeBrief(tree: string, task: string): string {
    return `${task}

Your working directory is the tree ${tree} of this workspace; the main tree owns the ports outside your block.`;
  }

  /** The tree an agent worked in, given back through `/v1`. Its work is on a branch by now. */
  async dropTree(name: string): Promise<void> {
    const t = this.trees.get(name);
    if (!t) return;
    this.trees.delete(name);
    await this.v1("DELETE", `/v1/workspaces/${encodeURIComponent(t.workspace)}/trees/${encodeURIComponent(t.tree)}`).catch(() => undefined);
    this.emit({ type: "tree-dropped", agent: name, workspace: t.workspace, tree: t.tree });
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

  /**
   * A person's create with no triple of its own takes the general default; one that NAMES a model
   * (a workspace session dispatching an agent) keeps the default where it is — `default: false`.
   */
  async create(body: Partial<Triple> & { default?: boolean } = {}): Promise<SessionRow> {
    this.refuse(true);
    // Only what the body names moves: an effort-only pick wiped the model once (Object.assign
    // copies an explicit undefined), and the footer read "no model" after the person chose one.
    const pick = Object.fromEntries(Object.entries({ model: body.model, thinking: body.thinking, effort: body.effort }).filter(([, v]) => v !== undefined)) as Triple;
    const named = pick.model !== undefined || pick.thinking !== undefined || pick.effort !== undefined;
    if (named && body.default !== false) this.write(() => this.defaults.set(pick));
    const t: Triple = { ...this.defaults.get(), ...Object.fromEntries(Object.entries(pick).filter(([, v]) => v !== undefined)) };
    const s = this.writable.run(() => this.sessions.create({ model: t.model ?? this.opts.model, thinking: t.thinking, effort: t.effort }));
    this.open(s);
    this.emit({ type: "sessions" });
    return s;
  }

  /**
   * A person's pick: the session's fields AND, unless the caller says otherwise, the general
   * default. Applied to the live child at once so the next turn uses it; effort reaches pi only at
   * the next start (the locked pi has no RPC for it — see `RpcChild.applyTriple`).
   */
  async setModel(id: string, body: Partial<Triple> & { default?: boolean }): Promise<SessionRow> {
    this.refuse(true);
    if (!this.sessions.get(id)) throw new Error(`no session ${id}`);
    // A model the provider cannot answer with is NOT a pick. Unvalidated, `deepseek/nope-9000` was
    // accepted with a 200, every turn then died with an empty `agent_end` (the provider's 400 went
    // only to the session file), and the bad id wrote through to the bench-wide default — so every
    // session created after it was born dead, and putting a good model back did not revive one
    // (api-test-report D1). Refused here, before anything is written.
    if (body.model !== undefined) {
      const known = await this.models().catch(() => undefined);
      const all = known?.providers.flatMap((p) => p.models.map((m) => `${p.id}/${m.id}`)) ?? [];
      // Only when the catalogue could actually be read: a bench with no child up yet lists nothing,
      // and refusing every pick because we could not ask is worse than the bug.
      if (all.length && !all.includes(body.model)) throw new NoSuchModel(body.model, all);
    }
    // Only what the body names moves: an effort-only pick wiped the model once (Object.assign
    // copies an explicit undefined), and the footer read "no model" after the person chose one.
    const pick = Object.fromEntries(Object.entries({ model: body.model, thinking: body.thinking, effort: body.effort }).filter(([, v]) => v !== undefined)) as Triple;
    if (body.default !== false) this.write(() => this.defaults.set(pick));
    const row = this.writable.run(() => this.sessions.update(id, pick));
    // A model CHANGE re-arms the child. `set_model` alone did not: a session poisoned by a bad pick
    // kept answering empty forever even after a good model was put back, while a fresh session on
    // the same model worked (api-test-report D1). The child is dropped and reopened, so the next
    // turn runs on a session that has actually taken the new model.
    const child = this.children.get(id);
    if (body.model !== undefined && child?.running()) {
      await child.stop().catch(() => undefined);
      this.children.delete(id);
      this.open(row);
    } else await child?.applyTriple(this.rowTriple(row));
    this.emit({ type: "sessions" });
    return row;
  }

  /**
   * The picker's catalogue: every provider pi supports, with the models of the ones a live child
   * can actually list. Answered from any running child — `get_available_models` is pi's own
   * snapshot of what the configured credentials reach, not a property of one session.
   */
  async models(): Promise<{ providers: { id: string; label: string; wired: boolean; models: { id: string; name: string; thinking: boolean; effort: boolean }[] }[] }> {
    const child = [...this.children.values()].find((c) => c.running());
    const rows = child ? await child.models().catch(() => []) : [];
    return { providers: allProviders().map((p) => ({ ...p, models: rows.filter((m) => m.provider === p.id).map(({ id, name, thinking, effort }) => ({ id, name, thinking, effort })) })) };
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
    /**
     * A slash line is a COMMAND, not something to say to a model. Over the rpc socket — a second
     * device, a script, anything that is not this desktop's composer — `/model`, `/clear` and
     * `/help` were forwarded verbatim: each burned a model turn explaining the bench has no slash
     * commands, and `/help` named the internal tool set to the person (api-test-report D5).
     *
     * The two the bench can honour are honoured; everything else is refused here. Nothing reaches
     * pi either way, so no slash line is ever in its context or its session file.
     */
    if ((cmd.type === "prompt" || cmd.type === "steer" || cmd.type === "follow_up") && msg.startsWith("/")) {
      const verb = msg.split(/\s+/)[0].toLowerCase();
      if (verb === "/clear") {
        const r = await this.rpc(id, { type: "new_session" });
        return r;
      }
      if (verb === "/compact") return this.rpc(id, { type: "compact" });
      return {
        type: "response",
        command: cmd.type,
        success: false,
        error: `${verb} is a desktop command; the bench does not take slash commands. Say what you want in words.`,
      } as PiEvent;
    }
    if (cmd.type === "prompt" && msg && !msg.startsWith("/") && s.name === `session ${s.seq}`) {
      this.write(() => this.sessions.update(id, { name: msg.replace(/\s+/g, " ").slice(0, 40), lastActive: Date.now() }));
      this.emit({ type: "sessions" });
    }
    const c = this.open(s);
    if (!c) throw new Error(`session ${id} has no file to open`);
    /**
     * A plain `prompt` while a turn is in flight is refused by pi, and the desktop's socket
     * (`WS /sessions/{id}/rpc`) sends exactly that — it is the ONLY prompt path, so a second line
     * 0.8 s after the first was simply dropped, on a warm child as well as a cold one
     * (api-test-report D2, regressed). Held as a follow-up instead, the way `send()` does it, so
     * the queue pi already owns takes it and nothing is lost.
     */
    /**
     * A PERSON's own line mid-turn is a STEER, first (spec 2026-09-17-bench-tools-no-fs §4: "a
     * person's own message is a steer, first"). This door is the desktop's socket, so that is who
     * is typing. Held as a follow-up it waited for a turn that might never end: a `[reply <id>]`
     * sat behind the very turn that was waiting for it, and a person adding a line to work in
     * flight was not read until that work finished.
     *
     * The fork-ordered queue is still the right home for a prompt from ANOTHER session — that
     * comes through `send()`, which sends a follow-up and lets pi's own queue take it.
     */
    if (cmd.type === "prompt" && this.turning.has(id)) {
      const r = await c.send({ ...cmd, type: "steer" });
      this.triageSoon(id);
      return r;
    }
    return c.send(cmd);
  }

  async messages(id: string, after?: number, limit?: number, tail?: number): Promise<{ messages: unknown[]; total: number; from: number }> {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    const c = this.children.get(id);
    if (c?.running()) {
      const r = await c.send({ type: "get_messages" });
      return page((r.data as { messages?: unknown[] } | undefined)?.messages ?? [], after, limit, tail);
    }
    return page(s.file && fs.existsSync(s.file) ? transcript(s.file) : [], after, limit, tail);
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
    // A session that is gone leaves nothing waiting on it and nothing drawn for it (spec §3.9):
    // its plan clears, its open asks settle, and the panels are told — an archived session's items
    // sitting in the plan panel is the person kept in the dark about work nobody is doing.
    this.write(() => this.plans.discard(id));
    this.emit({ type: "plan", session: id, items: [] });
    this.settleOpen(id, "cancelled", "the session was archived");
    this.emit({ type: "sessions" });
    return s;
  }

  /**
   * Every exchange this session is party to, ended: the ones it asked for, and the ones it was
   * holding. Nothing is left `running` with no session to answer it or hear the answer.
   */
  private settleOpen(session: string, state: string, _why: string): void {
    for (const e of this.exchanges.active()) {
      const holder = this.asked.get(session)?.some((q) => q.exchange === e.id);
      if (e.session !== session && !holder) continue;
      this.write(() => this.exchanges.transition(e.id, state));
      this.plan(e.session, { type: "ask_failed", exchange: e.id, task: e.workspace });
      this.emit({ type: "exchange", row: this.exchanges.bySession(e.session).find((x) => x.id === e.id) });
    }
    this.asked.delete(session);
    // A proposal this session raised has nobody to answer it any more (rule 2: never expires on its
    // own, cancelled when the session is archived).
    for (const [id, p] of this.proposals) if (p.session === session && !p.answer) this.answerProposal(id, "no");
  }

  async restore(id: string): Promise<SessionRow> {
    this.refuse(true);
    if (!this.sessions.get(id)) throw new Error(`no session ${id}`);
    const s = this.writable.run(() => this.sessions.update(id, { archived: false, lastActive: Date.now() }));
    this.open(s);
    this.emit({ type: "sessions" });
    return s;
  }

  /** What a name lookup found, and how long ago: a list call per ask is a round trip nobody needs. */
  private workspaces?: { at: number; rows: { id: string; name?: string }[] };

  /** Every id → name the bench already holds: the workspace list it caches, and its own sessions. */
  /** What to call a workspace in something a person reads: its name, else the id resolved, else the id. */
  private callIt(id: string | undefined, name?: string): string {
    if (name) return name;
    if (!id) return "that workspace";
    return this.knownNames()[id] ?? id;
  }

  private knownNames(): Record<string, string> {
    const out: Record<string, string> = {};
    for (const w of this.workspaces?.rows ?? []) if (w.name) out[w.id] = w.name;
    // A session names its own workspace even when the list has not been fetched in this process.
    for (const s of this.sessions.all()) if (s.workspace && s.name && !out[s.workspace]) out[s.workspace] = s.name;
    return out;
  }

  /**
   * A workspace NAME becomes its ID, once, before anything is spawned (owner, 2026-09-17: an
   * info-ask on "svelte-frontend" ran a fork whose `KL_TOOLS_WORKSPACE` was the name, so every tool
   * answered `workspace svelte-frontend: not found` and the fork had no transcript to read — the
   * session file is keyed by id too).
   *
   * A session this bench already holds settles it without asking anybody; otherwise the team's list
   * does, by exact id first and then by unique name. Ambiguous or unknown is ONE error, before a
   * child exists.
   */
  async resolveWorkspace(to: string): Promise<{ id: string; name: string }> {
    if (typeof to !== "string" || !to.trim()) throw new Error("which workspace?");
    const want = to.trim();
    // A thread we already hold names its own workspace, whichever way it was addressed.
    for (const key of [`w-${want}`, `e-${want}`]) {
      const s = this.sessions.get(key);
      if (s?.workspace) return { id: s.workspace, name: s.name || want };
    }
    const byWorkspace = this.sessions.all().find((s) => s.workspace === want);
    if (byWorkspace) return { id: want, name: byWorkspace.name || want };

    const fresh = this.workspaces && Date.now() - this.workspaces.at < 30_000;
    if (!fresh) {
      const list = this.opts.listWorkspaces ?? (() => import("../../pi/kloudlite.ts").then(async (m) => {
        const team = process.env.KL_TEAM;
        const r = await m.call("GET", `/v1/workspaces${team ? `?team=${encodeURIComponent(team)}` : ""}`);
        return Array.isArray(r.data) ? (r.data as { id: string; name?: string }[]) : [];
      }));
      this.workspaces = { at: Date.now(), rows: await list().catch(() => []) };
    }
    const rows = this.workspaces?.rows ?? [];
    // No list at all — offline, no team, a bench under test — is not evidence that the workspace is
    // wrong: take what was said as the id, which is what happened before any of this existed.
    if (!rows.length) return { id: want, name: want };
    const exact = rows.find((w) => w.id === want);
    if (exact) return { id: exact.id, name: exact.name || exact.id };
    const named = rows.filter((w) => w.name === want);
    if (named.length === 1) return { id: named[0].id, name: named[0].name || named[0].id };
    if (named.length > 1) throw new Error(`${want} is the name of ${named.length} workspaces (${named.map((w) => w.id).join(", ")}); say which id`);
    // Known list, and it is not in it: say what there is, once, before anything is spawned.
    throw new Error(`no workspace ${want} — this team has ${rows.map((w) => w.name ?? w.id).join(", ")}`);
  }

  /** The first version of the architecture document, from what this bench can already see. */
  async seedArchitecture(): Promise<void> {
    if (this.opts.readOnly || this.architecture.read().trim()) return;
    const list = this.opts.listWorkspaces ?? (() => import("../../pi/kloudlite.ts").then(async (m) => {
      const team = process.env.KL_TEAM;
      const r = await m.call("GET", `/v1/workspaces${team ? `?team=${encodeURIComponent(team)}` : ""}`);
      return Array.isArray(r.data) ? (r.data as { id: string; name?: string }[]) : [];
    }));
    // Never the bench itself: the document is what the model reads to learn what machines exist,
    // and a bench in it is a machine it will try to name (owner, 2026-09-18). `/v1` already hides
    // them; a row from an older api or a custom lister must not get through either.
    const own = process.env.KL_WORKSPACE_ID;
    const workspaces = (await list().catch(() => [])).filter((w) => !(w as { bench?: unknown }).bench && w.id !== own && !/^bench-[0-9a-f]{8,}$/.test(w.id));
    const services = await (this.opts.listServices?.() ?? Promise.resolve([])).catch(() => []);
    this.writable.run(() => this.architecture.seed({ workspaces, services }));
  }

  async openWorkspace(ws: string): Promise<SessionRow> {
    return this.openThread("workspace", ws);
  }

  async openEphemeral(ws: string, eph: string, tree?: string): Promise<SessionRow> {
    return this.openThread("ephemeral", ws, eph, tree);
  }

  private openThread(kind: "workspace" | "ephemeral", ws: string, eph?: string, tree?: string): SessionRow {
    // Checked before either id becomes a path.
    for (const x of kind === "workspace" ? [ws] : [ws, eph]) if (typeof x !== "string" || !WS_ID.test(x)) throw new Error(`not a workspace id: ${x}`);
    this.refuse(true);
    // Outside writable.run: a refusal there would read as a folder that cannot be written.
    this.sessions.threadId({ kind, workspace: ws, eph });
    const base = path.join(this.opts.dir, "workspaces", ws);
    const file = eph === undefined ? path.join(base, "thread.jsonl") : path.join(base, "eph", `${eph}.jsonl`);
    const s = this.writable.run(() => {
      fs.mkdirSync(path.dirname(file), { recursive: true });
      // An agent's hands are the WORKSPACE's, never its own name: `eph` is the session key and
      // nothing else. Binding tools to it made every agent's first command answer "workspace
      // <agent-name>: not found" and every kl_pkg_* 404 (owner, four agents, 2026-09-17).
      return this.sessions.thread({ kind, workspace: ws, eph, target: ws, file, model: this.opts.model, tree });
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
      // The session's own turn stops. Its PROCESSES do not: a process belongs to the WORKSPACE, not
      // to whichever session started it, and killing them here is what stopped the owner's dev
      // server between turns with exit 143 (2026-09-17). Only `stop: true` — a person asking for it
      // in as many words — reaches them.
      for (const t of this.tasks.all().filter((t) => t.session === id && (t.state === "running" || t.state === "background")))
        await c.send({ type: "prompt", message: t.state === "background" && t.n !== undefined ? `/cancel #${t.n}` : `/cancel ${t.id}` }).catch(() => undefined);
      if (stop) for (const p of this.procs.all().filter((p) => p.session === id && p.ended === undefined)) await this.killProc(id, p.id).catch(() => undefined);
      await c.send({ type: "abort" }).catch(() => undefined);
      c.stop();
    }
    this.children.delete(id);
    this.turning.delete(id);
    // A write failing midway leaves the child stopped but the row kept: it reopens on the next start and delete can be retried.
    this.writable.run(() => {
      for (const t of this.tasks.all().filter((t) => t.session === id && (t.state === "running" || t.state === "background"))) this.tasks.transition({ id: t.id, state: "cancelled", ended: Date.now() });
      this.exchanges.discard(id);
      this.plans.discard(id);
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
      if (!this.sessions.all().some((x) => !x.archived && x.id !== id && isBench(x))) this.open(this.sessions.create({ model: this.opts.model }));
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

  /**
   * A question ABOUT a workspace, answered without stopping it (§19). The workspace's own session
   * keeps its queue and its turn; a read-only FORK of it — its context, `read/grep/find/ls` on its
   * own tool server, nothing that writes — answers once and is thrown away. Many run at once.
   *
   * It is not work, so it is not a plan item: nobody is waiting on it to finish anything.
   */
  async infoAsk(to: string, question: string, from: string, timeoutMs = INFO_TIMEOUT_MS): Promise<{ exchange: string; workspace: string }> {
    this.refuse(true);
    if (typeof question !== "string" || !question.trim()) throw new Error("an info ask needs a question");
    if (!this.sessions.get(from)) throw new Error(`no session ${from}`);
    // The id first, before a row is written or a fork exists: a fork bound to a NAME has no tools
    // and no transcript, and it fails one tool call at a time instead of once, here.
    const { id: workspace, name } = await this.resolveWorkspace(to);
    const exchange = `info-${++this.askSeq}-${Date.now().toString(36)}`;
    const row = this.write(() => this.exchanges.record({ id: exchange, session: from, workspace, dir: "out", text: question, state: "running" }));
    this.emit({ type: "exchange", row });
    void this.answerInfo(workspace, question, from, exchange, timeoutMs, name).catch(() => this.transitionAsk({ exchange, from, workspace }, "failed"));
    return { exchange, workspace };
  }

  private async answerInfo(workspace: string, question: string, from: string, exchange: string, timeoutMs: number, name = workspace) {
    // Its own session's transcript is the context worth having; with none, the fork is a fresh
    // read-only session on that workspace — the tools, without the history.
    const target = this.sessions.get(`e-${workspace}`) ?? this.sessions.get(`w-${workspace}`);
    const file = target?.file && fs.existsSync(target.file) ? target.file : undefined;
    const id = `info-${Math.random().toString(36).slice(2, 8)}`;
    const forkDir = path.join(this.opts.dir, "btw", ".forks", id);
    fs.mkdirSync(forkDir, { recursive: true });
    let done!: () => void;
    const ended = new Promise<void>((r) => (done = r));
    const child = new RpcChild(id, { dir: forkDir, fork: file, info: true, tools: target?.target ?? workspace, model: target?.model ?? this.opts.model, bin: this.opts.bin, extDir: this.opts.extDir }, (ev) => {
      this.emit({ ...ev, pi: id });
      if (ev.type === "agent_end" || ev.type === "exit") done();
    });
    child.start();
    const timer = setTimeout(done, timeoutMs);
    timer.unref?.();
    try {
      const before = ((await child.send({ type: "get_messages" })).data as { messages?: unknown[] } | undefined)?.messages?.length ?? 0;
      await child.send({ type: "prompt", message: question });
      await ended;
      const all = (((await child.send({ type: "get_messages" })).data as { messages?: unknown[] } | undefined)?.messages ?? []) as { role?: string; content?: unknown }[];
      const last = [...all.slice(before)].reverse().find((m) => m.role === "assistant");
      const answer = typeof last?.content === "string" ? last.content : ((last?.content as { text?: string }[]) ?? []).map((c) => c.text ?? "").join("").trim();
      this.transitionAsk({ exchange, from, workspace }, answer ? "done" : "failed");
      const back = this.write(() => this.exchanges.record({ id: `${exchange}-in`, session: from, workspace, dir: "in", text: answer.slice(0, 2000), state: "done", ref: exchange }));
      this.emit({ type: "exchange", row: back });
      if (answer && this.sessions.get(from)) await this.send(from, `[info from ${name}] ${brief(answer, name)}`).catch(() => undefined);
    } finally {
      clearTimeout(timer);
      await child.stop();
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
