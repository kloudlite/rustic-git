//! The agent in-process behind the `Backend` interface: the pod's own TUI (cli.tsx) and the
//! remote host (serve.ts) both use it. The permission gate lives here as a hook that asks
//! `opts.permission`; the decision itself is the TUI's (modes, always-allow, the card).
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
import { Registry, platformTools, podTools, scratchRoot, scratchTools, webFetch, webSearch, type ToolDef } from "@kloudlite-tui/tools";
import { WORKSPACE_DIR, delegateTools, type DelegateDeps } from "./delegate.ts";
import { forgetSessions } from "./forget.ts";
import * as git from "./git.ts";
import { toolDiff } from "./diff.ts";
import { podfs } from "./podfs.ts";
import { space } from "./space.ts";
import { PROTOCOL } from "./wire.ts";
import type { Backend, CatalogModel, Hello, SessionHandle, SessionOpts } from "./index.ts";

/** Tools that ask before they run. */
export const GATED = new Set([
  "bash", "write", "edit", "patch", "exec", "web_fetch",
  "workspace_stop", "workspace_delete", "worktree_drop", "env_delete", "env_stop", "env_restore_in_place",
  "service_remove", "volume_delete", "snapshot_delete",
]);
/** Tools that only mutate the workspace's files — what acceptEdits waves through. */
export const EDITS = new Set(["write", "edit", "patch"]);

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
export function installGate(agent: any, permission: SessionOpts["permission"]): void {
  const ask = async (ctx: any, signal: AbortSignal) => {
    const name = ctx.toolCall.name;
    if (!GATED.has(name)) return undefined;
    const decision = await permission({ name, args: ctx.args, diff: toolDiff(name, ctx.args) ?? undefined }, signal);
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

/**
 * One agent per key per process, many views of it. The TUI and workspace_ask may hold one session
 * at once; a view going away must not end the other's turn, so only the last dispose runs `onLast`.
 */
export function shareable(base: SessionHandle, onLast: () => Promise<void>): () => SessionHandle {
  let count = 0;
  return () => {
    count++;
    const unsubs = new Set<() => void>();
    let gone = false;
    return {
      ...base,
      subscribe: (cb) => {
        const u = base.subscribe(cb);
        unsubs.add(u);
        return u;
      },
      dispose: async () => {
        if (gone) return;
        gone = true;
        for (const u of unsubs) u();
        if (--count === 0) await onLast();
      },
    };
  };
}

export class LocalBackend implements Backend {
  /** Open sessions, for workspace_ask to reach a running one; `busy` = mid-turn. */
  #live = new Map<string, SessionHandle>();
  #busy = new Set<string>();
  #views = new Map<string, () => SessionHandle>();

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

  async session(key: string, opts: SessionOpts): Promise<SessionHandle> {
    const model = resolveModel(opts.model);
    if (!model) throw new Error(`unknown model ${opts.model.provider}/${opts.model.id}`);
    const live = this.#live.get(key);
    const view = this.#views.get(key);
    // ponytail: a second opener inherits the first's tools/permission/model; rebuild the registry per view if the TUI's own tools must reach a session main opened.
    if (live && view && !opts.fresh) return view();
    await live?.dispose();
    const k = sessionKind(key);
    const deps = { live: this.#live, busy: this.#busy, open: (key: string, o: SessionOpts) => this.session(key, o) };
    const registry = await registryFor(k, deps, opts, key);
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
    installGate(agent, opts.permission);
    agent.subscribe((e: any) => {
      if (e.type === "agent_start") this.#busy.add(key);
      else if (e.type === "agent_end") this.#busy.delete(key);
    });
    const handle: SessionHandle = {
      messages: agent.messages,
      isClaude: "isClaude" in agent,
      prompt: async (text, o) => agent.prompt(text, o),
      steer: async (text, images) => agent.steer(text, images),
      followUp: async (text, images) => agent.followUp(text, images),
      clearQueue: async () => void agent.clearQueue(),
      abort: async () => agent.abort(),
      dispose: async () => {
        if (this.#live.get(key) === handle) {
          this.#live.delete(key);
          this.#views.delete(key);
        }
        this.#busy.delete(key);
        agent.dispose();
      },
      setModel: async (ref) => {
        const m = resolveModel(ref);
        if (!m) throw new Error(`unknown model ${ref.provider}/${ref.id}`);
        await agent.setModel(m);
      },
      setThinkingLevel: async (level) => void agent.setThinkingLevel(level),
      setAutoCompactionEnabled: async (on) => void agent.setAutoCompactionEnabled(on),
      subscribe: (cb) =>
        agent.subscribe((event: any) =>
          cb(
            event.type === "tool_execution_start"
              ? { ...event, diff: toolDiff(event.toolName, event.args) ?? undefined }
              : event,
          ),
        ),
    };
    this.#live.set(key, handle);
    const make = shareable(handle, () => handle.dispose());
    this.#views.set(key, make);
    return make();
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
