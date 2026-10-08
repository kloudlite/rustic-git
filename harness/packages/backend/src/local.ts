//! The agent in-process behind the `Backend` interface: the pod's own TUI (cli.tsx) and the
//! remote host (serve.ts) both use it. The permission gate lives here as a hook that asks
//! `opts.permission`; the decision itself is the TUI's (modes, always-allow, the card).
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
import { Registry, webFetch, webSearch } from "@kloudlite-tui/tools";
import * as git from "./git.ts";
import { toolDiff } from "./diff.ts";
import { PROTOCOL } from "./wire.ts";
import type { Backend, CatalogModel, Hello, SessionHandle, SessionOpts } from "./index.ts";

/** Tools that ask before they run. */
export const GATED = new Set(["bash", "write", "edit", "web_fetch"]);
/** Tools that only mutate the workspace's files — what acceptEdits waves through. */
export const EDITS = new Set(["write", "edit"]);

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

export class LocalBackend implements Backend {
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
      tools: [webFetch.name, webSearch.name],
    };
  }

  async session(key: string, opts: SessionOpts): Promise<SessionHandle> {
    const model = resolveModel(opts.model);
    if (!model) throw new Error(`unknown model ${opts.model.provider}/${opts.model.id}`);
    const agent: any = await createSession({
      key,
      model,
      registry: new Registry().add(webFetch, webSearch, ...opts.tools),
      fresh: opts.fresh,
      thinkingLevel: opts.thinkingLevel,
      autoCompact: opts.autoCompact,
      codemode: opts.codemode,
    });
    const inner = agent.agent.beforeToolCall;
    agent.agent.beforeToolCall = async (ctx: any, signal?: AbortSignal) => {
      const name = ctx.toolCall.name;
      if (GATED.has(name)) {
        const decision = await opts.permission(
          { name, args: ctx.args, diff: toolDiff(name, ctx.args) ?? undefined },
          signal ?? new AbortController().signal,
        );
        if (decision.block) return decision;
      }
      return inner?.(ctx, signal);
    };
    return {
      messages: agent.messages,
      isClaude: "isClaude" in agent,
      prompt: async (text, o) => agent.prompt(text, o),
      steer: async (text, images) => agent.steer(text, images),
      followUp: async (text, images) => agent.followUp(text, images),
      clearQueue: async () => void agent.clearQueue(),
      abort: async () => agent.abort(),
      dispose: async () => agent.dispose(),
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
  }

  sessions = {
    list: async (prefix?: string) => listSessions(prefix),
    name: async (key: string, name: string) => nameSession(key, name),
    describe: async (key: string, d: string) => describeSession(key, d),
    clear: async (key: string) => clearSessionHistory(key),
  };

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
