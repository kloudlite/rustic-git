import { mkdirSync, readdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import {
  DefaultResourceLoader,
  ModelRuntime,
  SessionManager,
  createAgentSession,
  createCodemodeExtension,
  getAgentDir,
  type AgentSession,
} from "@earendil-works/pi-coding-agent";
import type { Api, AuthInteraction, AuthType, Credential, Model } from "@earendil-works/pi-ai";
import type { Registry } from "@kloudlite-tui/tools";
import { CODEMODE_SKILL, KLOUDLITE_SKILL, roleSkill, type Role, claudeSignedIn, createClaudeSession, type ClaudeSession } from "./claude.ts";

export { claudeSignedIn, type ClaudeSession };
export { btwPrompt, piBtw } from "./btw.ts";

export type {
  AgentSession,
  AgentSessionEvent,
} from "@earendil-works/pi-coding-agent";
export type { AuthPrompt, AuthEvent, Message } from "@earendil-works/pi-ai";

const CONFIG_DIR =
  process.env.KLOUDLITE_CONFIG_DIR ?? join(homedir(), ".config", "kloudlite");

/**
 * Shared model/auth runtime (pi): every provider pi supports, models.json
 * custom providers, and credential storage (env keys, API keys, OAuth with
 * refresh) persisted under the kloudlite config dir.
 */
const runtime = await ModelRuntime.create({
  authPath: join(CONFIG_DIR, "auth.json"),
  modelsPath: join(CONFIG_DIR, "models.json"),
});

// ModelRuntime proxies the Models surface publicly (getModels/checkAuth/login/…)
export const models = runtime;

export type ModelRef = { provider: string; id: string };

export type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";

/** `thinking` is whether thinking blocks are shown; `thinkingLevel` is pi's reasoning budget. */
type Settings = { defaultModel?: ModelRef; theme?: string; sidebar?: "show" | "hide"; sidebarWidth?: number; thinking?: "show" | "hide"; thinkingLevel?: ThinkingLevel; autoCompact?: "on" | "off"; codemode?: "on" | "off"; vim?: "on" | "off" };

const SETTINGS_PATH = join(CONFIG_DIR, "settings.json");

export function readSettings(): Settings {
  try {
    return JSON.parse(readFileSync(SETTINGS_PATH, "utf8"));
  } catch {
    return {};
  }
}

export function writeSettings(patch: Partial<Settings>): void {
  mkdirSync(CONFIG_DIR, { recursive: true });
  writeFileSync(SETTINGS_PATH, JSON.stringify({ ...readSettings(), ...patch }, null, 2));
}

export function listModels(): { provider: string; id: string; name: string }[] {
  return models
    .getModels()
    .map((m) => ({ provider: m.provider, id: m.id, name: m.name ?? m.id }));
}

/**
 * Registry tools as pi custom tools. pi 1.0's signature is
 * `execute(toolCallId, params, …)` — params is the **second** argument.
 * Reading the first handed every tool its call id, so every declared parameter
 * arrived `undefined`. pi's tool type is cast through at the call site, so the
 * typechecker cannot catch a regression here; `tool-args.test.ts` is the guard.
 */
export function adaptTools(registry: Registry) {
  return registry.all().map((def) => ({
    name: def.name,
    label: def.name,
    description: def.description,
    parameters: def.inputSchema,
    execute: async (_toolCallId: string, args: unknown) => ({
      content: [{ type: "text" as const, text: await def.run(args) }],
    }),
  }));
}

export function resolveModel(ref: ModelRef): Model<Api> | undefined {
  return models.getModel(ref.provider, ref.id);
}

type ProviderAuth = {
  provider: string;
  /** Credentials resolve (env key, OAuth token, ambient) - a request can run. */
  ok: boolean;
  /** Env var(s) that would configure this provider, for the "how to add" hint. */
  envKeys: string[];
};

/**
 * The env vars a provider actually reads, asked of pi rather than guessed.
 * pi exports no mapping, but every provider resolves its credentials through
 * `ctx.env(name)` — so a no-op context records the names on the way past.
 * Guessing `${ID}_API_KEY` was wrong for 13 of 40 providers, and wrong in
 * ways that strand a user: Bedrock wants AWS credentials, Hugging Face wants
 * HF_TOKEN, github-copilot wants COPILOT_GITHUB_TOKEN.
 */
async function envKeysFor(provider: {
  auth?: { apiKey?: { resolve?: Function; check?: Function } };
}): Promise<string[]> {
  const asked: string[] = [];
  const ctx = {
    env: async (name: string) => {
      asked.push(name);
      return undefined;
    },
    fileExists: async () => false,
  };
  const input = { ctx, credential: undefined, signal: new AbortController().signal };
  for (const probe of [provider.auth?.apiKey?.resolve, provider.auth?.apiKey?.check]) {
    // a provider may resolve ambient credentials any way it likes; a throw
    // here just means it had nothing to ask for
    try {
      await probe?.(input);
    } catch {}
  }
  return [...new Set(asked)];
}

// Models read bash's prose ("Returns stdout and stderr") over its declared type and
// JSON.parse the result: one failed call plus two probes per session (2026-10-08). The same
// day the model retyped a 40-row table it already held. pi's codemode takes no extra guidance,
// so the rules ride on the description it prepares, PREPENDED: the first version appended
// them after the long API listing and the model skipped them.
const CODEMODE_NOTE =
  "Rules: (1) Call a tool directly for one action. Use codemode only for several calls, a loop over items, or to filter large output. " +
  "(2) `await tools.bash(...)` resolves to an object. Read `.output` (and `.exit_code`). Do not call string methods on the result itself. " +
  "(3) Fetch URLs with `tools.web_fetch`. Do not use curl in bash. Make one call for each item. Run the calls together with `Promise.all`. " +
  "(4) To show the user a table, list or report, build it in the script. Pass it to `tools.display({ markdown })`. The displayed text stays readable to you for follow-ups. Reply in one line. Do not retype what was displayed. " +
  "(5) `tools.searchTools(...)` and `tools.describeTool(...)` are asynchronous. Always use `await` with them. " +
  "(6) The shell is `tools.bash` in the main session and `tools.exec` in workspace sessions. Call the one that the tool list has. Do not guess. " +
  "(7) A poll loop exits as soon as the condition holds. It has a bounded count. It checks `services[].ready` (or `service_status[].ready`). It does not use a regex over the spec. A failed tool call throws. Do not poll after a write that you did not check. " +
  "The codemode skill has worked examples.";

// Markdown the running script asked to show, by root tool call id (nested ids are `<parent>/<n>`).
// Folded into the codemode result's details so the UI renders it under the script's card.
const displays = new Map<string, string[]>();
const DISPLAY_CAP = 200_000;

function displayTool() {
  return {
    name: "display",
    label: "Display",
    exposure: "codemode" as const,
    description:
      "Show markdown to the user. It renders in full under the card of this script. It also stays in the session for follow-ups. Do not retype it. Use it for finished tables, lists and reports.",
    parameters: {
      type: "object",
      properties: { markdown: { type: "string", description: "The markdown to show." } },
      required: ["markdown"],
    } as any,
    execute: async (toolCallId: string, params: { markdown: string }) => {
      const markdown = params.markdown ?? "";
      if (!markdown.trim()) throw new Error("display: markdown is empty");
      const root = toolCallId.split("/")[0]!;
      const shown = displays.get(root) ?? [];
      const total = shown.reduce((n, m) => n + m.length, 0);
      if (total + markdown.length > DISPLAY_CAP)
        return { content: [{ type: "text" as const, text: "display: limit reached, not shown" }], details: {} };
      displays.set(root, [...shown, markdown]);
      const text = `Shown to the user (${markdown.split("\n").length} lines); do not repeat it. Kept here for follow-ups:\n\n${markdown}`;
      return { content: [{ type: "text" as const, text }], details: {} };
    },
  };
}

export function withCodemodeExtras(factory: ReturnType<typeof createCodemodeExtension>): typeof factory {
  return (pi: any) => {
    pi.registerTool(displayTool());
    return factory(
      new Proxy(pi, {
        get: (t, k) =>
          k !== "registerTool"
            ? Reflect.get(t, k)
            : (tool: any) =>
                t.registerTool({
                  ...tool,
                  prepareLoadout: (loadout: any) => {
                    const r = tool.prepareLoadout(loadout);
                    if (r?.descriptions?.codemode) r.descriptions.codemode = `${CODEMODE_NOTE}\n\n${r.descriptions.codemode}`;
                    return r;
                  },
                  execute: async (id: string, params: any, signal: any, onUpdate: any, ...rest: any[]) => {
                    const live = (u: any) => {
                      const shown = displays.get(id);
                      return shown?.length ? { ...u, details: { ...u?.details, display: [...shown] } } : u;
                    };
                    try {
                      const result = await tool.execute(id, params, signal, onUpdate && ((u: any) => onUpdate(live(u))), ...rest);
                      const shown = displays.get(id);
                      if (!shown?.length) return result;
                      // the model sees only the script's return value, never its inner calls' results
                      // (live 2026-10-08: "none of the titles are in my context"), so the shown markdown
                      // rides on the codemode result itself: read once as input, never retyped as output
                      const kept = { type: "text" as const, text: `Shown to the user above; do not repeat it. Kept for follow-ups:\n\n${shown.join("\n\n")}` };
                      return { ...result, content: [...(result.content ?? []), kept], details: { ...result.details, display: shown } };
                    } finally {
                      displays.delete(id);
                    }
                  },
                }),
      }),
    );
  };
}

/** Auth status for every provider (env keys, stored credentials, ambient). */
export async function providerAuth(): Promise<ProviderAuth[]> {
  return Promise.all(
    models.getProviders().map(async (p) =>
      // Claude is served by Claude Code's own login, never by pi's credentials
      p.id === "anthropic"
        ? { provider: p.id, ok: await claudeSignedIn(), envKeys: [] }
        : {
            provider: p.id,
            ok: await models
              .checkAuth(p.id)
              .then((c) => c !== undefined)
              .catch(() => false),
            envKeys: await envKeysFor(p as never),
          },
    ),
  );
}

type LoginOption = {
  provider: string;
  /** "claude_code": sign-in happens in Claude Code (`kl-connect claude login`), not through pi. */
  type: AuthType | "claude_code";
  label: string;
};

/** Interactive login flows the providers offer (OAuth and prompted API keys). */
export function loginOptions(): LoginOption[] {
  const out: LoginOption[] = [];
  for (const p of models.getProviders()) {
    // Claude only through the Agent SDK: no pi oauth, and no api key (that would route through pi)
    if (p.id === "anthropic") {
      out.push({ provider: p.id, type: "claude_code", label: "Claude Code" });
      continue;
    }
    if (p.auth.oauth)
      out.push({
        provider: p.id,
        type: "oauth",
        label: p.auth.oauth.loginLabel ?? p.auth.oauth.name,
      });
    if (p.auth.apiKey?.login)
      out.push({ provider: p.id, type: "api_key", label: p.auth.apiKey.name });
  }
  return out;
}

export function loginProvider(
  provider: string,
  type: AuthType,
  interaction: AuthInteraction,
): Promise<Credential> {
  return models.login(provider, type, interaction);
}


/**
 * Create an agent session: pi's full harness - streaming, thinking, coding
 * tools (read/bash/edit/write), steering/follow-up queues, compaction,
 * auto-retry. Subscribe to `AgentSessionEvent`s for the UI.
 *
 * `key` scopes persistence: each session's history lives under
 * `~/.config/kloudlite/sessions/<key>` and is restored on the next start.
 */
/**
 * Archive a key's persisted transcripts (/clear): moves the session files into
 * an archive/ subdir so `continueRecent` starts from scratch, without deleting
 * anything.
 */
type SessionMeta = { key: string; name?: string; description?: string; updated: number };

function sessionDir(key: string): string {
  return join(CONFIG_DIR, "sessions", key.replace(/[^\w.-]/g, "_"));
}

function metaPath(key: string): string {
  return join(sessionDir(key), "meta.json");
}

function readMeta(key: string): SessionMeta | undefined {
  try {
    return JSON.parse(readFileSync(metaPath(key), "utf8"));
  } catch {
    return undefined;
  }
}

function writeMeta(meta: SessionMeta): void {
  mkdirSync(sessionDir(meta.key), { recursive: true });
  writeFileSync(metaPath(meta.key), JSON.stringify(meta, null, 2));
}

/** Give a session a human name, so it can be found and continued later. */
export function nameSession(key: string, name: string): void {
  writeMeta({ ...(readMeta(key) ?? { key, updated: Date.now() }), key, name });
}

/** One line on what this session is for, shown under its title. */
export function describeSession(key: string, description: string): void {
  writeMeta({ ...(readMeta(key) ?? { key, updated: Date.now() }), key, description });
}

/**
 * Persisted sessions, newest first. `key` is what `createSession` takes, so a
 * listed session can be continued as-is; `prefix` narrows to one context
 * (e.g. an environment's main sessions).
 */
export function listSessions(prefix = ""): SessionMeta[] {
  let dirs: string[];
  try {
    dirs = readdirSync(join(CONFIG_DIR, "sessions"));
  } catch {
    return [];
  }
  return dirs
    .map((d) => {
      try {
        return JSON.parse(readFileSync(join(CONFIG_DIR, "sessions", d, "meta.json"), "utf8")) as SessionMeta;
      } catch {
        return undefined;
      }
    })
    .filter((m): m is SessionMeta => !!m && m.key.startsWith(prefix))
    .sort((a, b) => b.updated - a.updated);
}

export function clearSessionHistory(key: string): void {
  const dir = sessionDir(key);
  let entries: string[];
  try {
    entries = readdirSync(dir);
  } catch {
    return;
  }
  const archive = join(dir, "archive");
  mkdirSync(archive, { recursive: true });
  for (const name of entries) {
    if (name === "archive" || name === "meta.json") continue;
    try {
      renameSync(join(dir, name), join(archive, `${Date.now()}-${name}`));
    } catch {}
  }
}

/**
 * Forget a deleted workspace's sessions: its own (`<ws>`, `<ws>:<id>`). Only after the platform accepted the delete, so a refused delete keeps
 * the history. Keys are sanitised into dir names (`:` becomes `_`); workspace ids never hold `_`.
 */
export function dropSessions(ws: string): void {
  const own = sessionDir(ws).split("/").pop()!;
  let dirs: string[];
  try {
    dirs = readdirSync(join(CONFIG_DIR, "sessions"));
  } catch {
    return;
  }
  for (const d of dirs)
    if (d === own || d.startsWith(`${own}_`)) rmSync(join(CONFIG_DIR, "sessions", d), { recursive: true, force: true });
}

/** Skill texts inlined into every pi session's system prompt: kloudlite, the role's, codemode's only while it is a tool. */
export function systemAppends(codemode?: boolean, role?: Role): string[] {
  return [KLOUDLITE_SKILL, ...(role ? [roleSkill(role)] : []), ...(codemode ? [CODEMODE_SKILL] : [])].map((d) => readFileSync(join(d, "SKILL.md"), "utf8"));
}

export async function createSession({
  key,
  model,
  registry,
  fresh = false,
  thinkingLevel,
  autoCompact,
  codemode,
  role,
  cwd = process.cwd(),
}: {
  key: string;
  model: Model<Api>;
  registry?: Registry;
  /** Start a brand-new persisted session instead of continuing the last one (/clear). */
  fresh?: boolean;
  /** pi's reasoning budget; a model that cannot think ignores it. */
  thinkingLevel?: ThinkingLevel;
  /** Let pi compact the context on its own when it fills up (default on). */
  autoCompact?: boolean;
  /** Let the model write a script that calls tools, instead of one call per turn. */
  codemode?: boolean;
  /** main or workspace: picks the role skill loaded beside kloudlite. */
  role?: Role;
  /** Working directory the model is told and Claude spawns in (default: this process's). */
  cwd?: string;
}): Promise<AgentSession | ClaudeSession> {
  // pi finds a session to continue by the cwd in its header, and every session written so far has
  // this process's cwd: keep it for the SessionManager so a changed `cwd` never orphans them.
  const smCwd = process.cwd();
  const dir = sessionDir(key);
  // meta.json makes a session findable later: its key, its name, last use
  // (Claude models and pi's own models share this key and transcript)
  writeMeta({ ...(readMeta(key) ?? { key }), key, updated: Date.now() });
  // pi's codemode ships as an extension and is registered *inactive*, so both
  // halves are needed: the factory on a resource loader, and the tool activated
  // by name. Passing `tools` up front would replace pi's whole default
  // allowlist, so the session is built with its defaults and `codemode` is
  // added to whatever `getActiveToolNames()` reports — hardcoding the four
  // built-ins silently cost `grep`, `find` and `ls`.
  // Always built: every session gets the Kloudlite skill inlined (pi lists skills only while a
  // read tool is active, and noTools "builtin" drops it). Without a loader pi builds
  // DefaultResourceLoader({cwd, agentDir, settingsManager}) with the same defaults used here.
  const resourceLoader = new DefaultResourceLoader({
    cwd,
    agentDir: getAgentDir(),
    // `mode: "on"`: tools stay declared directly beside `codemode`. A script is for many calls, a
    // loop, or filtering large output; one action is one direct call (owner ruling 2026-10-08).
    // An earlier "on" never fired codemode because nothing said when; the codemode skill and the
    // rules prepended to its description (CODEMODE_NOTE) now carry that.
    ...(codemode ? { extensionFactories: [{ name: "codemode", factory: withCodemodeExtras(createCodemodeExtension({ mode: "on" })) }] } : {}),
    // the same skills Claude sessions get through the plugin (claude.ts)
    appendSystemPrompt: systemAppends(codemode, role),
  });
  await resourceLoader.reload();
  const { session } = await createAgentSession({
    cwd,
    resourceLoader,
    modelRuntime: runtime,
    model,
    ...(thinkingLevel ? { thinkingLevel } : {}),
    // continue the most recent session in this key's dir (new file if none)
    sessionManager: fresh
      ? SessionManager.create(smCwd, dir)
      : SessionManager.continueRecent(smCwd, dir),
    // pi's read/bash/edit/write would run in the bench pod; real work goes through the pod tools
    noTools: "builtin",
    customTools: registry ? (adaptTools(registry) as never) : undefined,
  });
  // add codemode to pi's defaults rather than replacing them
  if (codemode) session.setActiveToolsByName([...session.getActiveToolNames(), "codemode"]);
  if (autoCompact !== undefined) session.setAutoCompactionEnabled(autoCompact);
  // Claude models run Claude Code's loop on this same session: its tools,
  // prompt and transcript (spec 2026-10-08-claude-tool-host). pi's own loop
  // never starts for them.
  if (model.provider === "anthropic") {
    const claude = createClaudeSession({ key, model, thinkingLevel, role, cwd, pi: session as never });
    if (autoCompact !== undefined) claude.setAutoCompactionEnabled(autoCompact);
    return claude;
  }
  return session;
}
