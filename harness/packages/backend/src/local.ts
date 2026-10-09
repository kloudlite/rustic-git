//! The agent in-process behind the `Backend` interface. The bench daemon (daemon.ts) holds the one
//! instance; every client (kl-tui over ssh, the browser TUI) is a connection to it, so an agent
//! outlives the client that started it and there is exactly one writer per session file.
//! One agent per key, many VIEWS of it (`shareable`): a view going away never ends a turn; an idle
//! agent with no views is disposed (`#settle`). The permission gate and the `question` tool raise
//! cards (cards.ts) to every connected TUI; the first answer wins.
import { mkdirSync } from "node:fs";
import { homedir } from "node:os";
import {
  claudeSignedIn,
  clearSessionHistory,
  createSession,
  describeSession,
  listSessions,
  loginOptions,
  loginProvider,
  models,
  nameSession,
  providerAuth,
  readSettings,
  piBtw,
  resolveModel,
  writeSettings,
  type ModelRef,
} from "@kloudlite-tui/agent";
import { Registry, platformTools, podFence, podTools, type PodFence, scratchRoot, scratchTools, webFetch, webSearch, type ToolDef } from "@kloudlite-tui/tools";
import { Cards } from "./cards.ts";
import { WORKSPACE_DIR, delegateTools, resumeAsks, type DelegateDeps } from "./delegate.ts";
import { forgetSessions } from "./forget.ts";
import * as git from "./git.ts";
import { BECAUSE_SCHEMA, TurnWords, consented } from "./consent.ts";
import { toolDiff } from "./diff.ts";
import { podfs } from "./podfs.ts";
import { space as spaceView } from "./space.ts";
import { readTasks, taskTools, tasksFile } from "./tasks.ts";
import { PROTOCOL } from "./wire.ts";
import type { Backend, BenchEvent, CatalogModel, Decision, PermMode, PermissionRequest, Hello, LiveSessionMeta, SessionEvent, SessionHandle, SessionOpts, SessionState, SpaceView, ThinkingLevel } from "./index.ts";

/** House actions: they change what the person owns (or the registry), so they ask whatever walls hold. */
export const ALWAYS_ASK = new Set([
  "workspace_stop", "workspace_delete", "worktree_drop", "env_delete", "env_stop", "env_restore_in_place",
  "service_remove", "volume_delete", "snapshot_delete", "container_push", "container_build",
  "packages_remove", "service_update", "intercept",
]);
/** Run code or reach the network: they ask unless the fence holds (see `mustAsk`). */
export const ASK_UNLESS_FENCED = new Set(["bash", "exec", "web_fetch"]);

/** Every tool `mustAsk` can be true for: each carries a required `because` (consent.ts). */
export const GATED = new Set([...ALWAYS_ASK, ...ASK_UNLESS_FENCED]);

/** A gated tool with `because` in its schema, removed before the tool runs: no tool ever sees it. */
export function asking(t: ToolDef): ToolDef {
  if (!GATED.has(t.name)) return t;
  const s: any = t.inputSchema ?? {};
  return {
    ...t,
    inputSchema: { ...s, type: "object", properties: { ...s.properties, because: BECAUSE_SCHEMA }, required: [...(s.required ?? []).filter((n: string) => n !== "because"), "because"] },
    run: (input: any) => {
      const { because: _, ...rest } = input ?? {};
      return t.run(rest);
    },
  };
}
/** Tools that only mutate the workspace's files — never asked: `paths::confine` / the scratch folder keep them in the tree. */
export const EDITS = new Set(["write", "edit", "patch"]);

/**
 * Whether a call needs a card. exec is behind a wall only when its pod's tool server reports both
 * a live sandbox and a fenced network; a missing field (older server, failed fetch) reads as open.
 * bash and web_fetch run in the bench process, whose network is fenced when KLOUDLITE_EGRESS says
 * so (bash's scratch sandbox already fails closed).
 */
export function mustAsk(name: string, fence?: PodFence, egress = process.env.KLOUDLITE_EGRESS, kind: "main" | "workspace" = "main"): boolean {
  if (kind === "workspace" && name === "workspace_stop") return false;
  if (ALWAYS_ASK.has(name)) return true;
  if (!ASK_UNLESS_FENCED.has(name)) return false;
  if (name === "exec") return !(fence?.sandbox === "active" && fence?.network === "fenced");
  return egress !== "fenced";
}

/** The person's own words: the first prompt carries the role card, which no view should show. */
export function stripCard(key: string, text: string): string {
  const card = `${roleCard(key)}\n\n`;
  return text.startsWith(card) ? text.slice(card.length) : text;
}

/** Who this session is, on its first message: Claude sessions see skills only by name until they
 * load one, so the role must arrive in the conversation itself (the system prompt stays Claude
 * Code's own for billing, claude.ts). */
export function roleCard(key: string): string {
  const k = sessionKind(key);
  if (k.kind === "main")
    return [
      "[role: main session]",
      "You orchestrate. You create, clone, start and delete workspaces and environments, keep the task board (task_add, task_update, task_list), and hand work to a workspace with workspace_ask (pass the task id).",
      "You never write the code yourself. Reports arrive as `[from <ws>] ...` messages; update the board from them and dispatch the next ready task.",
    ].join("\n");
  return [
    `[role: workspace session for ${k.ws}]`,
    "You work only in this workspace. Work comes from the person or as `[from main session] [task T<n>] ...`.",
    "You never create, clone, restore or delete workspaces. Report to main with main_tell: done, blocked, or need (for a fact or action from another workspace).",
    "Stop this workspace with workspace_stop only after you finished and main or the person said to stop.",
  ].join("\n");
}

export type SessionKind = { kind: "main" } | { kind: "workspace"; ws: string };

/** `main[:id]` is the main session, anything else a workspace's. */
export function sessionKind(key: string): SessionKind {
  const base = key.split(":")[0]!;
  if (base === "main") return { kind: "main" };
  return { kind: "workspace", ws: base };
}

function catalog(): CatalogModel[] {
  return models.getModels().map((m) => ({ provider: m.provider, id: m.id, name: m.name ?? m.id, input: [...m.input] }));
}

function defaultModel(list: CatalogModel[]): ModelRef {
  // An explicitly chosen model wins outright — not validated against the catalog: on a cold
  // start only pi's bundled seed is known, and validating discarded any model it predates.
  const saved = readSettings().defaultModel;
  if (saved) return saved;
  const opus = list.find((m) => m.provider === "anthropic" && /opus/.test(m.id));
  return opus ?? list[0] ?? { provider: "anthropic", id: "claude-opus-5" };
}

function forgetting(t: ToolDef, deps: DelegateDeps): ToolDef {
  return {
    ...t,
    async run(input: { workspace: string }) {
      const r = await t.run(input);
      if (typeof r === "string" && !r.startsWith("error") && !r.startsWith("platform tools unavailable")) await forgetSessions(input.workspace, deps.live);
      return r;
    },
  };
}

/** Asks the person a multiple-choice question as a card; never gated (the card is the ask). */
function question(key: string, cards: Cards): ToolDef {
  return {
    name: "question",
    description: "Ask the user a question and wait for their answer. Use it when you need a decision or clarification. Give 2-5 short answer options.",
    inputSchema: { type: "object", properties: { question: { type: "string", description: "The question to ask." }, options: { type: "array", items: { type: "string" }, description: "The answer options that the user can select." } }, required: ["question", "options"] },
    run: async ({ question, options }: { question: string; options: string[] }) => {
      // ponytail: Tool.run carries no signal, so only dispose/delete (withdrawKey) ends a pending question; thread a signal through ToolDef to also end it on interrupt
      const picked = await cards.ask({ key, kind: "question", tool: "question", title: question, options: options.map((label, i) => ({ id: String(i), label })) }, new AbortController().signal, "__withdrawn");
      if (picked === "__withdrawn") throw new Error("the question was withdrawn (turn interrupted)");
      return options[Number(picked)] ?? picked;
    },
  } as ToolDef;
}

/** Who gets which hands: main reaches the platform, delegates and has a confined scratch folder (bash, read, write); a workspace session has the
 * pod's code tools and its own slice of the platform, and reports to main with main_tell. */
export async function registryFor(k: SessionKind, deps: DelegateDeps, opts: SessionOpts, key = "main"): Promise<Registry> {
  const r = new Registry();
  if (k.kind === "main")
    return r.add(...[webFetch, webSearch, ...platformTools("main").map((t) => (t.name === "workspace_delete" ? forgetting(t, deps) : t)), ...delegateTools("main", undefined, deps, opts, key), ...taskTools(deps.tasks ?? tasksFile()), ...scratchTools(scratchRoot(key)), ...opts.tools].map(asking), ...(deps.cards ? [question(key, deps.cards)] : []));
  // the self-stop is the one call that never asks: it only snapshots and parks the workspace
  // that is already finished, and carries no `because` for a card to quote
  return r.add(...[webFetch, webSearch, ...(await podTools(k.ws)), ...platformTools("workspace", k.ws), ...delegateTools("workspace", k.ws, deps, opts), ...opts.tools].map((t) => (t.name === "workspace_stop" ? t : asking(t))), ...(deps.cards ? [question(key, deps.cards)] : []));
}

/** The permission gate, on both doors a call comes through: `agent.beforeToolCall` for a
 * model's own calls (pi's loop and Claude's tool server both call it), and the pi session's
 * `_beforeToolCall` for calls a codemode script makes, which pi's nested runner sends straight
 * there with the parent's id and no signal. Gated once per call: top level has no parent id. */
export function installGate(agent: any, permission: SessionOpts["permission"], fence?: () => PodFence | undefined, words?: () => { typed: string[]; self?: string }, kind: "main" | "workspace" = "main"): void {
  const ask = async (ctx: any, signal: AbortSignal) => {
    const name = ctx.toolCall.name;
    if (!mustAsk(name, fence?.(), undefined, kind)) return undefined;
    const { because, ...args } = ctx.args ?? {};
    const w = words?.();
    if (w && consented(name, args, because, w.typed, w.self)) return undefined;
    const said = (v: unknown) => (typeof v === "string" && v.trim() ? v.trim() : undefined);
    const decision = await permission!(
      { name, args, diff: toolDiff(name, args) ?? undefined, reason: said(because?.reason), claimed: said(because?.asked) },
      signal,
    );
    return decision.block ? decision : undefined;
  };
  const inner = agent.agent.beforeToolCall;
  agent.agent.beforeToolCall = async (ctx: any, signal?: AbortSignal) =>
    (await ask(ctx, signal ?? new AbortController().signal)) ?? inner?.(ctx, signal);
  const pi = agent.pi ?? agent;
  const nested = pi._beforeToolCall?.bind(pi);
  if (!nested) return;
  // ponytail: nested calls get no abort signal from pi, so an abort mid-card leaves the card up
  // until answered; thread the codemode call's signal through if that bites.
  pi._beforeToolCall = async (ctx: any, parentId?: string) =>
    (parentId && (await ask(ctx, new AbortController().signal))) || nested(ctx, parentId);
}

/**
 * Workspace sessions work in the pod's ~/workspace, so the model is told that path and
 * resolves relative paths there; main keeps the bench's cwd. Every file tool is the pod's, so on the
 * bench the folder only has to exist (Claude spawns its process in it; session() creates it).
 */
export function sessionCwd(k: { kind: string }): string | undefined {
  return k.kind === "main" ? undefined : WORKSPACE_DIR;
}

/** A copy of `src` whose live fields stay live: a spread would freeze `busy` and `messages` at the
 * moment the view was made, and a client reconnecting to a running agent reads both. */
function derive(src: SessionHandle, over: Partial<SessionHandle>): SessionHandle {
  return Object.defineProperties({ ...src, ...over }, {
    busy: { get: () => src.busy, enumerable: true },
    state: { get: () => src.state, enumerable: true },
    messages: { get: () => src.messages, enumerable: true },
  }) as SessionHandle;
}

/**
 * One agent per key per process, many views of it. The TUI and workspace_ask may hold one session
 * at once; a view going away must not end the other's turn, so views never dispose the agent:
 * `onZero` runs (not awaited) whenever the last one is gone and the owner decides (`#settle`).
 */
export function shareable(base: SessionHandle, onZero: () => void): { view: () => SessionHandle; count: () => number } {
  let count = 0;
  const view = () => {
    count++;
    const unsubs = new Set<() => void>();
    let gone = false;
    return derive(base, {
      subscribe: (cb) => {
        const u = base.subscribe(cb);
        unsubs.add(u);
        return u;
      },
      dispose: async () => {
        if (gone) return;
        gone = true;
        for (const u of unsubs) u();
        if (--count === 0) onZero();
      },
    });
  };
  return { view, count: () => count };
}

/**
 * The agent's own handle. ONE `agent.subscribe` fans out to `subs`, so disposing the agent can tell
 * every subscriber (`session_closed`): a dispose drops the agent's listeners and no `agent_end`
 * follows, which left workspace_ask waiting forever on a turn that was gone.
 */
export function baseHandle(
  agent: any,
  key: string,
  hooks: { busy: Set<string>; onEnd(): void; onDispose(): void; onChange?(): void; state: SessionState; rebuild?(): Promise<void> },
): SessionHandle & { close(reopen: boolean): Promise<void> } {
  const subs = new Set<(e: SessionEvent) => void>();
  let closed = false;
  const state = hooks.state;
  const emit = (e: SessionEvent) => {
    for (const cb of [...subs]) {
      try {
        cb(e);
      } catch (err) {
        console.error("session subscriber failed", key, err);
      }
    }
  };
  const pushState = () => emit(state);
  /** `reopen` tells every view the session lives on (rebuilt): reattach, don't treat it as gone. */
  const close = async (reopen = false) => {
    if (closed) return;
    closed = true;
    for (const cb of [...subs]) cb({ type: "session_closed", reopen });
    subs.clear();
    hooks.onDispose();
    hooks.busy.delete(key);
    if (typeof unsub === "function") unsub();
    agent.dispose();
  };
  const unsub = agent.subscribe((event: any) => {
    if (event.type === "agent_start") hooks.busy.add(key);
    else if (event.type === "agent_end") hooks.busy.delete(key);
    if (event.type === "agent_start" || event.type === "agent_end") hooks.onChange?.();
    let e = event.type === "tool_execution_start" ? { ...event, diff: toolDiff(event.toolName, event.args) ?? undefined } : event;
    if (e.type === "message_start" && e.message?.role === "user") {
      const text = (e.message.content ?? []).filter((b: any) => b.type === "text").map((b: any) => b.text).join("\n");
      e = { ...e, shown: stripCard(key, text) };
    }
    emit(e);
    if (event.type === "queue_update") {
      state.queued = { steering: [...(event.steering ?? [])], followUp: [...(event.followUp ?? [])] };
      pushState();
    } else if (event.type === "message_end" && event.message?.role === "assistant") {
      state.tokens += event.message.usage?.totalTokens ?? 0;
      pushState();
    }
    if (event.type === "agent_end") hooks.onEnd();
  });
  return {
    get messages() {
      return agent.messages.map((m: any) => {
        if (m.role !== "user" || !Array.isArray(m.content)) return m;
        const i = m.content.findIndex((b: any) => b.type === "text");
        if (i < 0) return m;
        const content = m.content.slice();
        content[i] = { ...content[i], text: stripCard(key, content[i].text) };
        return { ...m, content };
      });
    },
    get busy() {
      return hooks.busy.has(key);
    },
    get state() {
      return state;
    },
    isClaude: "isClaude" in agent,
    prompt: async (text, o) => agent.prompt(agent.messages.length === 0 ? `${roleCard(key)}\n\n${text}` : text, o),
    steer: async (text, images) => agent.steer(text, images),
    followUp: async (text, images) => agent.followUp(text, images),
    clearQueue: async () => void agent.clearQueue(),
    btw: (q) => ("isClaude" in agent ? agent.btw(q) : piBtw(agent, q)),
    abort: async () => agent.abort(),
    dispose: () => close(false),
    close,
    setModel: async (ref) => {
      const m = resolveModel(ref);
      if (!m) throw new Error(`unknown model ${ref.provider}/${ref.id}`);
      if ((ref.provider === "anthropic") !== "isClaude" in agent) {
        // Claude and pi are different agents: crossing means a rebuild from the stored state
        if (hooks.busy.has(key)) throw new Error("a turn is running");
        state.model = { provider: ref.provider, id: ref.id };
        pushState();
        return hooks.rebuild?.();
      }
      await agent.setModel(m);
      state.model = { provider: ref.provider, id: ref.id };
      pushState();
    },
    setThinkingLevel: async (level) => {
      agent.setThinkingLevel(level);
      state.thinkingLevel = level;
      pushState();
    },
    setAutoCompactionEnabled: async (on) => {
      agent.setAutoCompactionEnabled(on);
      state.autoCompact = on;
      pushState();
    },
    setCodemode: async (on) => {
      if (state.codemode === on) return;
      if (hooks.busy.has(key)) throw new Error("a turn is running");
      state.codemode = on;
      pushState();
      await hooks.rebuild?.();
    },
    subscribe: (cb) => (subs.add(cb), () => void subs.delete(cb)),
  };
}

export class LocalBackend implements Backend {
  /** Open sessions (the agents' base handles), for workspace_ask to reach a running one. */
  #live = new Map<string, ReturnType<typeof baseHandle>>();
  #busy = new Set<string>();
  #watchers = new Set<(list: LiveSessionMeta[]) => void>();
  #list = () => listSessions().map((m) => ({ ...m, busy: this.#busy.has(m.key) }));
  /** Every view's sidebar: the stored list with which keys are mid-turn right now. */
  #changed() {
    if (this.#watchers.size === 0) return;
    const list = this.#list();
    for (const cb of [...this.#watchers]) {
      try {
        cb(list);
      } catch (err) {
        console.error("sessions watcher failed", err);
      }
    }
  }
  #shared = new Map<string, ReturnType<typeof shareable>>();
  /** What each live agent was built with: a later opener that needs more rebuilds it. */
  #built = new Map<string, { tools: Set<string>; agent: any }>();
  /** Per key, outlives the agent: a rebuild reads it back. Never deleted on dispose; `fresh` replaces it. */
  #state = new Map<string, SessionState>();
  readonly #create: typeof createSession;
  constructor(o: { create?: typeof createSession } = {}) {
    this.#create = o.create ?? createSession;
  }
  /** Keys to rebuild at their next `agent_end` (an opener needed a different build mid-turn). */
  #rebuildAtEnd = new Set<string>();
  /** Per key chain: opens and rebuilds run one at a time. */
  #opening = new Map<string, Promise<unknown>>();
  #serial<T>(key: string, fn: () => Promise<T>): Promise<T> {
    // one at a time per key: an open racing a rebuild must find the rebuilt agent, never build a
    // second one on the same session file (one writer per file)
    const prev = this.#opening.get(key) ?? Promise.resolve();
    const next = prev.catch(() => {}).then(fn);
    this.#opening.set(key, next);
    void next.finally(() => this.#opening.get(key) === next && this.#opening.delete(key)).catch(() => {});
    return next;
  }

  /** test seam: mark a key busy without a model turn */
  busyForTest(key: string, on: boolean) {
    on ? this.#busy.add(key) : this.#busy.delete(key);
  }

  /** Dispose the live agent so every view reopens it; the next open rebuilds from #state. `force`
   * skips the busy refusal (clear aborts first). */
  #rebuild(key: string, force = false): Promise<void> {
    return this.#serial(key, async () => {
      if (!force && this.#busy.has(key)) throw new Error("a turn is running");
      const live = this.#live.get(key);
      this.#built.delete(key);
      await live?.close(true);
    });
  }
  /** Permission mode for every TUI; per daemon process, so a restart resets it. */
  #mode: PermMode = "default";
  #bench = new Set<(e: BenchEvent) => void>();
  #cards = new Cards((e) => this.#broadcast(e));
  #broadcast(e: BenchEvent) {
    for (const f of this.#bench) f(e);
  }
  /** What the person typed through client views, per key (consent.ts). */
  #typed = new Map<string, TurnWords>();
  #words(key: string): TurnWords {
    let w = this.#typed.get(key);
    if (!w) this.#typed.set(key, (w = new TurnWords()));
    return w;
  }

  /** Turns running now: the bench's idle clock must not stop a pod under one (daemon.ts). */
  get busyCount(): number {
    return this.#busy.size;
  }

  async hello(): Promise<Hello> {
    const list = catalog();
    return {
      protocol: PROTOCOL,
      settings: readSettings(),
      catalog: list,
      defaultModel: defaultModel(list),
      logins: loginOptions(),
      sessions: listSessions(),
      cwd: process.cwd(),
      home: homedir(),
      tools: [webFetch.name, webSearch.name, ...platformTools("main").map((t) => t.name), "workspace_ask", "task_add", "task_update", "task_list", "bash", "read", "write", "question"],
      asks: this.#cards.pending(),
      mode: this.#mode,
    };
  }

  /** Tools that only mutate the workspace's files — what acceptEdits waves through. */
  static readonly EDITS = new Set(["write", "edit", "patch"]);

  /** The gate's decision: the mode first, then a card every connected TUI hears. "always" answers like "once" here; the grant is the TUI's. */
  async permit(key: string, { name, args, diff, session, reason, claimed }: PermissionRequest, signal: AbortSignal): Promise<Decision> {
    // a delegated session asks through its caller: the card belongs to the caller's key
    const asker = session ?? key;
    const mode = this.#mode;
    // plan mode answers rather than asks: a refusal the model can read beats a card every turn
    if (mode === "plan")
      return { block: true, reason: `Plan mode: ${name} is not available. Research and explain what you would do; the user will leave plan mode when they want it done.` };
    if (mode === "bypass" || (mode === "acceptEdits" && LocalBackend.EDITS.has(name))) return {};
    const why = reason ? `Why: ${reason}` : claimed ? `Says you asked: “${claimed}”, which is not in your messages this turn` : "No reason given";
    const subtitle = name === "bash" ? "Shell command" : name === "web_fetch" ? "Fetch a URL" : LocalBackend.EDITS.has(name) ? `${name === "write" ? "Write" : "Edit"} ${args?.path ?? "file"}` : `Run ${name}`;
    const detail = name === "bash" ? `$ ${args?.command ?? ""}` : name === "web_fetch" ? String(args?.url ?? "") : diff ? undefined : JSON.stringify(args ?? {}).slice(0, 400);
    const choice = await this.#cards.ask({
      key: asker, kind: "permission", tool: name, title: "Permission required", subtitle,
      body: [why, detail].filter(Boolean).join("\n\n"), diff,
      options: [{ id: "once", label: "Allow once" }, { id: "always", label: "Allow always" }, { id: "reject", label: "Reject" }],
    }, signal, "reject");
    return choice === "reject" ? { block: true, reason: "The user rejected this tool call." } : {};
  }

  async watch(cb: (e: BenchEvent) => void): Promise<() => void> {
    this.#bench.add(cb);
    return () => void this.#bench.delete(cb);
  }
  asks: Backend["asks"] = { answer: async (id, choice) => this.#cards.answer(id, choice) };
  mode: Backend["mode"] = {
    set: async (m) => {
      this.#mode = m;
      this.#broadcast({ type: "perm", mode: m });
    },
  };

  /** main_tell's routing: who asked each workspace last, and which already reported this ask. */
  #lastCaller = new Map<string, string>();
  #reported = new Set<string>();

  #deps(): DelegateDeps {
    return {
      lastCaller: this.#lastCaller,
      reported: this.#reported,
      live: this.#live,
      busy: this.#busy,
      open: (key, o) => this.session(key, o),
      permit: (key, req, signal) => this.permit(key, req, signal),
      cards: this.#cards,
    };
  }

  /** Boot: resend the asks a restart interrupted. Returns once they are started, not finished. */
  async resumeAsks(): Promise<void> {
    void Promise.allSettled(resumeAsks(this.#deps()));
  }

  /** Dispose the agent when no turn runs and nobody views it (or it is due a rebuild). Called from
   * the last view going away and, a tick after `agent_end`, from the agent itself: the tick lets
   * `answer()` see the `agent_end` first and lets Claude start a queued followUp (busy again). */
  #settle(key: string) {
    const base = this.#live.get(key);
    const sh = this.#shared.get(key);
    if (!base || !sh || this.#busy.has(key)) return;
    if (this.#rebuildAtEnd.has(key)) void base.close(true);
    else if (sh.count() === 0) void base.close(false);
  }

  /** Register this view's client (card callback + TUI tools) for its key; the view's dispose removes it. */
  #attach(key: string, opts: SessionOpts, v: SessionHandle): SessionHandle {
    if (!opts.client) return v;
    const typed = opts.client
      ? {
          prompt: async (text: string, o?: Parameters<SessionHandle["prompt"]>[1]) => {
            this.#words(key).add(text);
            if (!listSessions().find((m) => m.key === key)?.name) await this.sessions.name(key, text.replace(/\s+/g, " ").trim().slice(0, 40));
            return v.prompt(text, o);
          },
          steer: (text: string, images?: Parameters<SessionHandle["steer"]>[1]) => (this.#words(key).add(text), v.steer(text, images)),
          followUp: (text: string, images?: Parameters<SessionHandle["followUp"]>[1]) => (this.#words(key).add(text), v.followUp(text, images)),
        }
      : {};
    return derive(v, {
      ...typed,
      dispose: async () => {
        await v.dispose();
      },
    });
  }

  session(key: string, opts: SessionOpts): Promise<SessionHandle> {
    return this.#serial(key, () => this.#open(key, opts));
  }

  async #open(key: string, opts: SessionOpts): Promise<SessionHandle> {
    // a rebuild for a missing tool keeps what the person set; `fresh` and first builds take `initial`
    const prev = opts.fresh ? undefined : this.#state.get(key);
    const want = prev?.model ?? opts.initial?.model ?? defaultModel(catalog());
    const settings = readSettings();
    const initial = {
      thinkingLevel: prev?.thinkingLevel ?? opts.initial?.thinkingLevel ?? ((settings.thinkingLevel ?? "medium") as ThinkingLevel),
      autoCompact: prev?.autoCompact ?? opts.initial?.autoCompact ?? (settings.autoCompact ?? "on") === "on",
      codemode: prev?.codemode ?? opts.initial?.codemode ?? (settings.codemode ?? "on") === "on",
    };
    const live = this.#live.get(key);
    const sh = this.#shared.get(key);
    const built = this.#built.get(key);
    if (live && sh && built && !opts.fresh) {
      // The opener's model, thinking level and codemode are never applied here: the daemon's state is
      // the truth and the opener reads it back (`state`). Only a tool it needs and the agent lacks rebuilds.
      if (opts.tools.length === 0) return this.#attach(key, opts, sh.view());
      if (!opts.tools.some((t) => !built.tools.has(t.name))) return this.#attach(key, opts, sh.view());
      if (live.busy) {
        // rebuilt at its agent_end; until then this opener sees the old agent
        this.#rebuildAtEnd.add(key);
        return this.#attach(key, opts, sh.view());
      }
    }
    await live?.close(true); // other views get session_closed and reopen
    const model = resolveModel(want);
    if (!model) throw new Error(`unknown model ${want.provider}/${want.id}`);
    const k = sessionKind(key);
    const registry = await registryFor(k, this.#deps(), opts, key);
    const cwd = sessionCwd(k);
    // best effort: off the bench (a laptop running the backend) / is not writable, and only Claude spawns there
    if (cwd) try { mkdirSync(cwd, { recursive: true }); } catch {}
    const agent: any = await this.#create({
      key,
      model,
      registry,
      fresh: opts.fresh,
      thinkingLevel: initial.thinkingLevel,
      autoCompact: initial.autoCompact,
      codemode: initial.codemode,
      role: k.kind,
      cwd,
    });
    installGate(agent, opts.permission ?? ((req, signal) => this.permit(key, req, signal)), k.kind === "main" ? undefined : () => podFence(k.ws), () => ({ typed: this.#words(key).get(), self: k.kind === "main" ? undefined : k.ws }), k.kind);
    const state: SessionState = {
      type: "session_state",
      model: { provider: want.provider, id: want.id },
      ...initial,
      queued: { steering: [], followUp: [] },
      tokens: prev?.tokens ?? 0,
    };
    this.#state.set(key, state);
    const handle: ReturnType<typeof baseHandle> = baseHandle(agent, key, {
      busy: this.#busy,
      state,
      onEnd: () => void setTimeout(() => this.#settle(key), 0),
      onChange: () => this.#changed(),
      rebuild: () => this.#rebuild(key),
      onDispose: () => {
        if (this.#live.get(key) !== handle) return;
        this.#live.delete(key);
        this.#shared.delete(key);
        this.#built.delete(key);
        this.#rebuildAtEnd.delete(key);
        this.#cards.withdrawKey(key);
        // dispose clears busy right after this hook, so report once that has happened
        queueMicrotask(() => this.#changed());
      },
    });
    handle.subscribe((e) => {
      if (e.type === "agent_start") this.#words(key).start();
      else if (e.type === "agent_end") this.#words(key).end();
    });
    this.#live.set(key, handle);
    this.#built.set(key, { tools: new Set(opts.tools.map((t) => t.name)), agent });
    const made = shareable(handle, () => this.#settle(key));
    this.#shared.set(key, made);
    this.#changed();
    return this.#attach(key, opts, made.view());
  }

  sessions = {
    list: async (prefix?: string) => listSessions(prefix),
    name: async (key: string, name: string) => (await nameSession(key, name), this.#changed()),
    describe: async (key: string, d: string) => (await describeSession(key, d), this.#changed()),
    clear: async (key: string) => {
      // abort first if mid-turn (as the TUI's /clear did), close the agent, then archive: every view
      // reopens against the emptied history
      if (this.#busy.has(key)) await this.#live.get(key)?.abort();
      await this.#rebuild(key, true);
      await clearSessionHistory(key);
      const st = this.#state.get(key);
      if (st) (st.tokens = 0), (st.queued = { steering: [], followUp: [] });
      this.#changed();
    },
    watch: async (cb: (list: LiveSessionMeta[]) => void) => {
      this.#watchers.add(cb);
      cb(this.#list());
      return () => void this.#watchers.delete(cb);
    },
  };

  space = async (): Promise<SpaceView> => ({ ...(await spaceView()), tasks: readTasks(tasksFile()) });

  settings = {
    write: async (patch: Parameters<typeof writeSettings>[0]) => {
      const out = await writeSettings(patch);
      if (patch.codemode) {
        const on = patch.codemode === "on";
        for (const key of [...this.#live.keys()]) {
          const st = this.#state.get(key);
          if (!st || st.codemode === on) continue;
          st.codemode = on;
          if (this.#busy.has(key)) this.#rebuildAtEnd.add(key);
          else await this.#rebuild(key).catch(() => {});
        }
      }
      return out;
    },
  };

  models = {
    refresh: async () => {
      await models.refresh();
      return catalog();
    },
  };

  auth = {
    providers: () => providerAuth(),
    login: async (provider: string, type: any, ui: any) => {
      await loginProvider(provider, type, ui);
      // an agent built before the sign-in holds the old (missing) credentials
      for (const key of [...this.#live.keys()])
        if (this.#state.get(key)?.model.provider === "anthropic" && !this.#busy.has(key)) await this.#rebuild(key).catch(() => {});
    },
    claudeSignedIn: async (fresh?: boolean) => claudeSignedIn(fresh),
  };

  podfs = podfs;

  fs = {
    isGitRepo: async (root: string) => git.isGitRepo(root),
    changes: async (root: string) => git.changes(root),
    fileDiff: async (root: string, p: string, s: git.ChangeStatus) => git.fileDiff(root, p, s),
    fullFile: async (root: string, p: string, s?: git.ChangeStatus) => git.fullFile(root, p, s),
    listDir: async (root: string, rel: string) => git.listDir(root, rel),
    grep: async (root: string, q: string, limit?: number) => git.grep(root, q, limit),
  };
}

// Until the TUI stops importing git helpers from here (Task 6).
export * from "./git.ts";
export { toolDiff } from "./diff.ts";
