//! The agent in-process behind the `Backend` interface. The bench daemon (daemon.ts) holds the one
//! instance; every client (kl-tui over ssh, the browser TUI) is a connection to it, so an agent
//! outlives the client that started it and there is exactly one writer per session file.
//! One agent per key, many VIEWS of it (`shareable`): a view going away never ends a turn; an idle
//! agent with no views is disposed (`#settle`). The permission gate and the TUI's own tools route by
//! session key to the newest connected client (clients.ts); the decision itself is the TUI's.
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
  resolveModel,
  writeSettings,
  type ModelRef,
} from "@kloudlite-tui/agent";
import { Registry, platformTools, podFence, podTools, type PodFence, scratchRoot, scratchTools, webFetch, webSearch, type ToolDef } from "@kloudlite-tui/tools";
import { Clients } from "./clients.ts";
import { WORKSPACE_DIR, delegateTools, resumeAsks, type DelegateDeps } from "./delegate.ts";
import { forgetSessions } from "./forget.ts";
import * as git from "./git.ts";
import { toolDiff } from "./diff.ts";
import { podfs } from "./podfs.ts";
import { space } from "./space.ts";
import { PROTOCOL } from "./wire.ts";
import type { Backend, CatalogModel, Hello, SessionEvent, SessionHandle, SessionOpts } from "./index.ts";

/** House actions: they change what the person owns (or the registry), so they ask whatever walls hold. */
export const ALWAYS_ASK = new Set([
  "workspace_stop", "workspace_delete", "worktree_drop", "env_delete", "env_stop", "env_restore_in_place",
  "service_remove", "volume_delete", "snapshot_delete", "container_push",
]);
/** Run code or reach the network: they ask unless the fence holds (see `mustAsk`). */
export const ASK_UNLESS_FENCED = new Set(["bash", "exec", "web_fetch"]);
/** Tools that only mutate the workspace's files — never asked: `paths::confine` / the scratch folder keep them in the tree. */
export const EDITS = new Set(["write", "edit", "patch"]);

/**
 * Whether a call needs a card. exec is behind a wall only when its pod's tool server reports both
 * a live sandbox and a fenced network; a missing field (older server, failed fetch) reads as open.
 * bash and web_fetch run in the bench process, whose network is fenced when KLOUDLITE_EGRESS says
 * so (bash's scratch sandbox already fails closed).
 */
export function mustAsk(name: string, fence?: PodFence, egress = process.env.KLOUDLITE_EGRESS): boolean {
  if (ALWAYS_ASK.has(name)) return true;
  if (!ASK_UNLESS_FENCED.has(name)) return false;
  if (name === "exec") return !(fence?.sandbox === "active" && fence?.network === "fenced");
  return egress !== "fenced";
}

export type SessionKind = { kind: "main" } | { kind: "workspace"; ws: string } | { kind: "subagent"; ws: string };

/** `main[:id]` is the main session, `<ws>:agent-<hex>` a subagent, anything else a workspace's. */
export function sessionKind(key: string): SessionKind {
  const base = key.split(":")[0]!;
  if (base === "main") return { kind: "main" };
  return key.includes(":agent-") ? { kind: "subagent", ws: base } : { kind: "workspace", ws: base };
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

/** Who gets which hands: main reaches the platform, delegates and has a confined scratch folder (bash, read, write); a workspace session has the
 * pod's code tools and its own slice of the platform; a subagent only code tools. */
export async function registryFor(k: SessionKind, deps: DelegateDeps, opts: SessionOpts, key = "main"): Promise<Registry> {
  const r = new Registry();
  if (k.kind === "main")
    return r.add(webFetch, webSearch, ...platformTools("main").map((t) => (t.name === "workspace_delete" ? forgetting(t, deps) : t)), ...delegateTools("main", undefined, deps, opts, key), ...scratchTools(scratchRoot(key)), ...opts.tools);
  if (k.kind === "workspace")
    return r.add(webFetch, webSearch, ...(await podTools(k.ws)), ...platformTools("workspace", k.ws), ...delegateTools("workspace", k.ws, deps, opts), ...opts.tools);
  return r.add(webFetch, ...(await podTools(k.ws)));
}

/** The permission gate, on both doors a call comes through: `agent.beforeToolCall` for a
 * model's own calls (pi's loop and Claude's tool server both call it), and the pi session's
 * `_beforeToolCall` for calls a codemode script makes, which pi's nested runner sends straight
 * there with the parent's id and no signal. Gated once per call: top level has no parent id. */
export function installGate(agent: any, permission: SessionOpts["permission"], fence?: () => PodFence | undefined): void {
  const ask = async (ctx: any, signal: AbortSignal) => {
    const name = ctx.toolCall.name;
    if (!mustAsk(name, fence?.())) return undefined;
    const decision = await permission!({ name, args: ctx.args, diff: toolDiff(name, ctx.args) ?? undefined }, signal);
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
 * Workspace and subagent sessions work in the pod's ~/workspace, so the model is told that path and
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
  hooks: { busy: Set<string>; onEnd(): void; onDispose(): void },
): SessionHandle {
  const subs = new Set<(e: SessionEvent) => void>();
  let closed = false;
  const unsub = agent.subscribe((event: any) => {
    if (event.type === "agent_start") hooks.busy.add(key);
    else if (event.type === "agent_end") hooks.busy.delete(key);
    const e = event.type === "tool_execution_start" ? { ...event, diff: toolDiff(event.toolName, event.args) ?? undefined } : event;
    for (const cb of [...subs]) {
      try {
        cb(e);
      } catch (err) {
        console.error("session subscriber failed", key, err);
      }
    }
    if (event.type === "agent_end") hooks.onEnd();
  });
  return {
    get messages() {
      return agent.messages;
    },
    get busy() {
      return hooks.busy.has(key);
    },
    isClaude: "isClaude" in agent,
    prompt: async (text, o) => agent.prompt(text, o),
    steer: async (text, images) => agent.steer(text, images),
    followUp: async (text, images) => agent.followUp(text, images),
    clearQueue: async () => void agent.clearQueue(),
    abort: async () => agent.abort(),
    dispose: async () => {
      if (closed) return;
      closed = true;
      for (const cb of [...subs]) cb({ type: "session_closed" });
      subs.clear();
      hooks.onDispose();
      hooks.busy.delete(key);
      if (typeof unsub === "function") unsub();
      agent.dispose();
    },
    setModel: async (ref) => {
      const m = resolveModel(ref);
      if (!m) throw new Error(`unknown model ${ref.provider}/${ref.id}`);
      await agent.setModel(m);
    },
    setThinkingLevel: async (level) => void agent.setThinkingLevel(level),
    setAutoCompactionEnabled: async (on) => void agent.setAutoCompactionEnabled(on),
    subscribe: (cb) => (subs.add(cb), () => void subs.delete(cb)),
  };
}

export class LocalBackend implements Backend {
  /** Open sessions (the agents' base handles), for workspace_ask to reach a running one. */
  #live = new Map<string, SessionHandle>();
  #busy = new Set<string>();
  #shared = new Map<string, ReturnType<typeof shareable>>();
  /** What each live agent was built with: a later opener that needs more rebuilds it. */
  #built = new Map<string, { claude: boolean; codemode: boolean; tools: Set<string>; agent: any }>();
  /** Keys to rebuild at their next `agent_end` (an opener needed a different build mid-turn). */
  #rebuild = new Set<string>();
  #clients = new Clients();
  /** Unregisters of the clients each key's views added; a closed agent has no client left. */
  #offs = new Map<string, Set<() => void>>();

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
      tools: [webFetch.name, webSearch.name, ...platformTools("main").map((t) => t.name), "workspace_ask", "subagent", "bash", "read", "write"],
    };
  }

  #deps(): DelegateDeps {
    return {
      live: this.#live,
      busy: this.#busy,
      open: (key, o) => this.session(key, o),
      permit: (key, req, signal) => this.#clients.route(key, (c) => !!c.permission, (c) => c.permission!(req, signal), signal),
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
    if (sh.count() === 0 || this.#rebuild.has(key)) void base.dispose();
  }

  /** Register this view's client (card callback + TUI tools) for its key; the view's dispose removes it. */
  #attach(key: string, opts: SessionOpts, v: SessionHandle): SessionHandle {
    if (!opts.permission && opts.tools.length === 0) return v;
    const off = this.#clients.add(key, { permission: opts.permission, tools: new Map(opts.tools.map((t) => [t.name, t.run as (input: unknown) => Promise<string>])) });
    let offs = this.#offs.get(key);
    if (!offs) this.#offs.set(key, (offs = new Set()));
    offs.add(off);
    return derive(v, {
      dispose: async () => {
        off();
        offs.delete(off);
        await v.dispose();
      },
    });
  }

  async session(key: string, opts: SessionOpts): Promise<SessionHandle> {
    const model = resolveModel(opts.model);
    if (!model) throw new Error(`unknown model ${opts.model.provider}/${opts.model.id}`);
    const live = this.#live.get(key);
    const sh = this.#shared.get(key);
    const built = this.#built.get(key);
    if (live && sh && built && !opts.fresh) {
      // An internal open (a reply delivered to main, an ask reaching a workspace) carries none of the
      // TUI's tools: it takes the agent as the person left it. Its model and codemode are the asker's,
      // and applying them would undo a model switch or rebuild the session under the person's TUI.
      if (opts.tools.length === 0) return this.#attach(key, opts, sh.view());
      const needsRebuild =
        (opts.model.provider === "anthropic") !== built.claude ||
        !!opts.codemode !== built.codemode ||
        opts.tools.some((t) => !built.tools.has(t.name));
      if (!needsRebuild) {
        const cur = built.agent.model;
        if (cur && (cur.provider !== opts.model.provider || cur.id !== opts.model.id)) await live.setModel(opts.model);
        if (opts.thinkingLevel && opts.thinkingLevel !== built.agent.thinkingLevel) await live.setThinkingLevel(opts.thinkingLevel);
        return this.#attach(key, opts, sh.view());
      }
      if (live.busy) {
        // rebuilt at its agent_end; until then this opener sees the old agent
        this.#rebuild.add(key);
        return this.#attach(key, opts, sh.view());
      }
    }
    await live?.dispose(); // other views get session_closed
    const k = sessionKind(key);
    // the TUI's own tools run in whichever client is connected to this key NOW, not the opener
    const routed: SessionOpts = {
      ...opts,
      tools: opts.tools.map((t) => ({
        ...t,
        run: (input: unknown) => this.#clients.route(key, (c) => c.tools.has(t.name), (c) => c.tools.get(t.name)!(input)),
      })),
    };
    const registry = await registryFor(k, this.#deps(), routed, key);
    const cwd = sessionCwd(k);
    // best effort: off the bench (a laptop running the backend) / is not writable, and only Claude spawns there
    if (cwd) try { mkdirSync(cwd, { recursive: true }); } catch {}
    const agent: any = await createSession({
      key,
      model,
      registry,
      fresh: opts.fresh,
      thinkingLevel: opts.thinkingLevel,
      autoCompact: opts.autoCompact,
      codemode: opts.codemode,
      cwd,
    });
    installGate(agent, (req, signal) => this.#clients.route(key, (c) => !!c.permission, (c) => c.permission!(req, signal), signal), k.kind === "main" ? undefined : () => podFence(k.ws));
    const handle: SessionHandle = baseHandle(agent, key, {
      busy: this.#busy,
      onEnd: () => void setTimeout(() => this.#settle(key), 0),
      onDispose: () => {
        if (this.#live.get(key) !== handle) return;
        this.#live.delete(key);
        this.#shared.delete(key);
        this.#built.delete(key);
        this.#rebuild.delete(key);
        for (const off of this.#offs.get(key) ?? []) off();
        this.#offs.delete(key);
      },
    });
    this.#live.set(key, handle);
    this.#built.set(key, { claude: "isClaude" in agent, codemode: !!opts.codemode, tools: new Set(opts.tools.map((t) => t.name)), agent });
    const made = shareable(handle, () => this.#settle(key));
    this.#shared.set(key, made);
    return this.#attach(key, opts, made.view());
  }

  sessions = {
    // subagent sessions are throwaway: never offered for resume
    list: async (prefix?: string) => listSessions(prefix).filter((s) => !s.key.includes(":agent-")),
    name: async (key: string, name: string) => nameSession(key, name),
    describe: async (key: string, d: string) => describeSession(key, d),
    clear: async (key: string) => clearSessionHistory(key),
  };

  space = space;

  settings = { write: async (patch: Parameters<typeof writeSettings>[0]) => writeSettings(patch) };

  models = {
    refresh: async () => {
      await models.refresh();
      return catalog();
    },
  };

  auth = {
    providers: () => providerAuth(),
    login: async (provider: string, type: any, ui: any) => void (await loginProvider(provider, type, ui)),
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
