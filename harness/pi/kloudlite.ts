import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "typebox";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { OperationErrorCode } from "../bench/src/operations/contracts.ts";
import type { JsonValue } from "../bench/src/operations/shape.ts";
import { workspaceCreateSourceIssues } from "../bench/src/operations/arguments.ts";
import type { ArgumentStates } from "../bench/src/operations/arguments.ts";
import { createSharedAdapters, resolveUnique, resolveWorkspaceProgress, toolResult as adapterToolResult } from "../bench/src/operations/adapters.ts";
import { TOOLS, gated, question } from "./catalog.ts";

/**
 * The bench's hands on the platform: `/v1` as tools. Authentication is the
 * person's own: a short-lived platform token the desktop app mints and the
 * platform projects into this pod at `KL_TOOL_TOKEN_FILE`, re-read on every
 * call so a refresh lands without a restart and a revoked session stops at the
 * next call. Nothing here holds a credential of its own, and every write and
 * delete is named as such in the catalogue so a person can decide what the model may do.
 */
const SIGN_IN = "sign in on the Kloudlite desktop app";

function token(): { api: string; token: string } {
  const f = process.env.KL_TOOL_TOKEN_FILE;
  const api = process.env.KL_API_URL;
  let t = "";
  try {
    t = f ? fs.readFileSync(f, "utf8").trim() : "";
  } catch {
    /* unreadable is the same as absent */
  }
  if (!t || !api) throw new Error(SIGN_IN);
  return { api: api.replace(/\/$/, ""), token: t };
}

export async function call(method: string, p: string, body?: unknown, signal?: AbortSignal): Promise<{ status: number; data: unknown }> {
  const c = token();
  const r = await fetch(`${c.api}${p}`, {
    method,
    redirect: "error",
    headers: { authorization: `Bearer ${c.token}`, ...(body !== undefined ? { "content-type": "application/json" } : {}) },
    body: body !== undefined ? JSON.stringify(body) : undefined,
    signal,
  });
  const text = await r.text();
  if (r.status === 401) return { status: 401, data: `${SIGN_IN} (your desktop session ended or the bench was stopped)` };
  // The api answers JSON; a page of HTML means the request never reached it
  // (an unpublished route, a redirect to sign in) — say that, not the page.
  if (/^\s*<!doctype html|^\s*<html/i.test(text)) return { status: r.status >= 400 ? r.status : 502, data: `not an api answer for ${p} — the route is not published on ${c.api} (got a web page)` };
  let data: unknown = text;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    /* not json: the text is the answer (a 403's sentence among them) */
  }
  return { status: r.status, data };
}

/** What every tool answers with, here and in `workspace-tools.ts`. */
export type ToolResult = { content: { type: "text"; text: string }[]; isError?: boolean };

const text = (v: unknown): ToolResult => ({ content: [{ type: "text" as const, text: typeof v === "string" ? v : JSON.stringify(v, null, 2) }] });

/**
 * How a dispatch ended. `refused` means policy stopped the call before the
 * handler ran, so no effect exists to reconcile and the executor must not read
 * it as success. A handler that ran and answered with an error is `failed`.
 */
export type DispatchOutcome<T = ToolResult> =
  | { outcome: "completed"; result: T }
  | { outcome: "failed"; code: OperationErrorCode; result: T }
  | { outcome: "refused"; code: OperationErrorCode; reason: string };

export type ApprovalRequirement = "none" | "user" | "policy";

export const DECLINED = "declined by the person";
const NO_APPROVAL_CHANNEL = "that change needs approval and no approval channel is available; nothing ran";

/**
 * One policy-bearing dispatch for every caller: the registered tools (a person
 * is asked asynchronously through `propose`) and the operation executor's
 * capabilities. Scope/path/argument refusals run first, then approval, then the
 * handler — in that order, so a call that cannot be authorized never reaches it,
 * and a mutating plan with no approval channel is refused rather than run.
 */
export type PolicyPlan<T = ToolResult> = {
  capability: string;
  effect: "read" | "write" | "destroy";
  approval: { required: ApprovalRequirement; obtain?: () => Promise<boolean> };
  inspect?: () => { code: OperationErrorCode; reason: string } | undefined;
  run: () => Promise<T>;
  failed?: (result: T) => OperationErrorCode | undefined;
  /** The answer a thrown handler error becomes; the registered tools keep `thrown`'s sentence. */
  error?: (e: unknown) => ToolResult;
};

export async function dispatchWithPolicy<T = ToolResult>(plan: PolicyPlan<T>): Promise<DispatchOutcome<T>> {
  const blocked = plan.inspect?.();
  if (blocked) return { outcome: "refused", code: blocked.code, reason: blocked.reason };
  if (plan.approval.required !== "none") {
    const obtain = plan.approval.obtain;
    if (!obtain) return { outcome: "refused", code: "permission_denied", reason: NO_APPROVAL_CHANNEL };
    const granted = await obtain().catch((error) => {
      if (plan.capability.includes(".")) throw error;
      return false;
    });
    if (!granted) return { outcome: "refused", code: "permission_denied", reason: DECLINED };
  }
  try {
    const result = await plan.run();
    const code = plan.failed?.(result) ?? ((result as ToolResult)?.isError ? "execution_failure" : undefined);
    return code ? { outcome: "failed", code, result } : { outcome: "completed", result };
  } catch (e) {
    if (!plan.error) throw e;
    const answer = plan.error(e);
    return { outcome: "failed", code: "execution_failure", result: { ...answer, isError: true } as T };
  }
}

/**
 * The registered tool surface keeps the words the model already reads: a
 * decline is its own sentence, a scope/argument refusal is an error, and an
 * error the handler answered with is unchanged. The executor reads the
 * `DispatchOutcome` itself, where a denial is never "completed".
 */
export function toolResultOf(outcome: DispatchOutcome<ToolResult>, declined: "error" | "plain" = "error"): ToolResult {
  if (outcome.outcome === "completed") return outcome.result;
  if (outcome.outcome === "failed") return { ...outcome.result, isError: true };
  if (declined === "plain" && outcome.code === "permission_denied" && outcome.reason === DECLINED) {
    return { content: [{ type: "text", text: outcome.reason }] };
  }
  return { content: [{ type: "text", text: outcome.reason }], isError: true };
}

/**
 * What a failed platform call SAYS to the model: one sentence about the person's intent, and
 * nothing about where anything runs. A model told "502: not an api answer for /v1/repos — the
 * route is not published on https://dev.kloudlite.io" repeated all of it to the person
 * (owner, 2026-09-18), which is a host, a route and a status code they can do nothing with — and
 * which the identity (§3.5) says the session does not know in the first place.
 *
 * The detail is not lost: it goes to the bench's log on stderr, named by the tool, where a person
 * debugging the platform can read it.
 */
/**
 * Tools that answer a LIST. A 404 from one of these is not "it does not exist" — there is no name
 * in the request to be wrong about: it is an empty collection, or a route this token cannot reach.
 * `kl_images` answered "that images does not exist; check the name with the person", which is a
 * sentence about nothing (owner, 2026-09-18).
 */
const LISTS = new Set(["kl_images", "kl_workspaces", "kl_environments", "kl_repos", "kl_pkg_list", "kl_workspace_snapshots", "kl_environment_snapshots", "kl_env_current"]);

export function sanitizeError(tool: string, status: number, data: unknown): string {
  const raw = typeof data === "string" ? data : JSON.stringify(data);
  // The one message that IS the person's to act on, and says nothing about the platform's shape.
  if (status === 401 || String(raw).includes(SIGN_IN)) return SIGN_IN;
  // stderr, never the tool result: the bench captures this, the model never sees it.
  try {
    process.stderr.write(`kl-tool ${tool} ${status} ${String(raw).slice(0, 500)}\n`);
  } catch {
    /* a closed stderr is not worth failing a tool call over */
  }
  const thing = subject(tool);
  if (status === 403) return `you are not allowed to do that with ${thing}; the person may have to grant it`;
  if (status === 404)
    return LISTS.has(tool)
      ? `no ${thing.replace(/^the /, "")} to list here yet`
      : `that ${thing.replace(/^the /, "")} does not exist; check the name with the person`;
  if (status === 409) return `that cannot be done while ${thing} is in its current state; say what you tried`;
  if (status === 422) return `${thing} refused those details: ${short(raw)}`;
  if (status >= 500) return `${thing} could not be reached just now; try again, or tell the person it is unavailable`;
  return `${thing} refused that: ${short(raw)}`;
}

/** A thrown error, said the same way a refused call is: never a host, a route or a status. */
export function thrown(tool: string, e: Error): string {
  const said = String(e?.message ?? "");
  if (said.includes(SIGN_IN)) return SIGN_IN;
  const clean = short(said);
  try {
    process.stderr.write(`kl-tool ${tool} threw ${said.slice(0, 500)}\n`);
  } catch {
    /* a closed stderr is not worth failing a tool call over */
  }
  return clean === "it did not say why" ? `${subject(tool)} could not be reached just now; try again, or tell the person it is unavailable` : clean;
}

/** What a tool is ABOUT, in a person's words: `kl_workspace_create` → "workspaces". */
function subject(tool: string): string {
  const name = tool.replace(/^kl_/, "");
  if (name.startsWith("repo") || name.startsWith("pull")) return "repositories";
  if (name.startsWith("workspace") || name.startsWith("pkg")) return "workspaces";
  if (name.startsWith("env")) return "environments";
  if (name.startsWith("container") || name.startsWith("image")) return "images";
  if (name.startsWith("volume") || name.startsWith("snapshot")) return "snapshots";
  return "the platform";
}

/**
 * The sentence a 4xx body carries, with anything that names the platform's shape taken out: a URL,
 * a route, a host, a port or a bare status code is never the person's business.
 */
function short(raw: unknown): string {
  const said = String(raw ?? "")
    .replace(/https?:\/\/\S+/g, "")
    .replace(/\/v\d+\/\S*/g, "")
    .replace(/\b\d{1,3}(\.\d{1,3}){3}(:\d+)?\b/g, "")
    .replace(/\b[a-z0-9-]+\.[a-z0-9.-]+\.[a-z]{2,}\b/gi, "")
    .replace(/\b[45]\d\d\b/g, "")
    .replace(/[{}"\[\]]/g, " ")
    .replace(/\s{2,}/g, " ")
    .trim();
  return said.slice(0, 160) || "it did not say why";
}

const answer = async (method: string, p: string, body?: unknown, tool = "") => {
  const { status, data } = await call(method, p, body);
  if (status >= 400) return { ...text(sanitizeError(tool || p.split("/")[2] || "the platform", status, data)), isError: true };
  return text(data ?? "done");
};
/**
 * A service as `/v1` takes it. `command`, `env` and `mounts` have no serde default on the api
 * side, so an omitted one is a 422 rather than an empty list — `service()` fills them in.
 */
const SERVICE = Type.Object({
  name: Type.String(),
  image: Type.String(),
  command: Type.Optional(Type.Array(Type.String(), { description: "overrides the image's entrypoint command" })),
  env: Type.Optional(Type.Record(Type.String(), Type.String(), { description: "environment variables" })),
  mounts: Type.Optional(Type.Array(Type.Object({ folder: Type.String({ description: "one safe segment of the environment's own volume" }), path: Type.String({ description: "where it is mounted in the container" }) }))),
  ports: Type.Optional(Type.Array(Type.Number(), { description: "container ports siblings reach it on by name" })),
});
const service = (s: Record<string, any>) => ({ name: s.name, image: s.image, command: s.command ?? [], env: s.env ?? {}, mounts: s.mounts ?? [], ports: s.ports ?? [] });

/**
 * Wait for the platform, so the model does not. A create, a start, a restore and a push all answer
 * 202 and finish later; with no way to wait, a model runs `bash "sleep 20; echo waited"` and then
 * guesses (the fleet, 2026-09-17). This polls the route's own GET until `done` says so, honouring
 * the abort the tool call carries, and answers what it last saw either way — a tool that timed out
 * still hands back the real document, never an error and never a lie.
 */
export async function settle(
  get: () => Promise<{ status: number; data: unknown }>,
  done: (d: any) => boolean,
  capMs: number,
  signal?: AbortSignal,
  everyMs = 2_000,
  now: () => number = Date.now,
  sleep: (ms: number) => Promise<void> = (ms) => new Promise((r) => setTimeout(r, ms)),
): Promise<{ data: unknown; waitedMs: number; settled: boolean }> {
  const began = now();
  for (;;) {
    const r = await get();
    const waitedMs = now() - began;
    // A refusal is an answer: nothing is going to change by asking again.
    if (r.status >= 400 || done(r.data)) return { data: r.data, waitedMs, settled: r.status < 400 };
    if (signal?.aborted || waitedMs + everyMs > capMs) return { data: r.data, waitedMs, settled: false };
    await sleep(everyMs);
  }
}

const q = (o: Record<string, string | undefined>) => {
  const s = new URLSearchParams(Object.entries(o).filter(([, v]) => v) as [string, string][]).toString();
  return s ? `?${s}` : "";
};

/**
 * What the model is: the Kloudlite harness, and nothing else. This REPLACES pi's
 * own system prompt rather than appending to it (owner, 2026-09-17): a bench
 * session has no filesystem and no shell of its own, so a coding-agent prompt
 * about local files, the CLI it happens to be built on, or paths under
 * /opt/harness describes a machine it cannot touch and invites it to go looking.
 * Its tools are the whole world it sees — which is also why it is told not to try them out: a
 * model with no filesystem answers "what can you do?" by calling something, and every kl_* write
 * lands on the person's real workspaces.
 */
/**
 * The caveman compression rules, vendored beside this file. Read once at load: the extension dir is
 * wherever the harness tree was installed, and a missing file is a prompt without the style rather
 * than an extension that will not start.
 */
function caveman(): string[] {
  try {
    // @ts-expect-error The extension loader executes this file as ESM; the focused contract
    // typecheck follows the desktop's CommonJS package boundary even though pi is not built by it.
    const at = path.join(path.dirname(fileURLToPath(import.meta.url)), "caveman.md");
    const body = fs.readFileSync(at, "utf8").trim();
    return body
      ? [
          // "Chat text only" was read as "the reply only", so every ask and report crossed as a
          // paragraph with a lead-in and a restatement of what the receiver already held (owner,
          // 2026-09-18). Code, files and commits stay normal prose; a message to another SESSION is
          // not prose, it is a note between two things that already share the context.
          "Speak in the caveman style below. It applies to what you say to the person AND to every message that crosses to another session — `ask`'s task and brief, `report`'s text, an agent's brief and its report: no preamble, no restating what they already hold, no \"context:\" section. Code, files and commits stay normal prose.",
          body,
        ]
      : [];
  } catch {
    return [];
  }
}
const CAVEMAN = caveman();

export type Audience = "bench" | "workspace";

export function identity(hands: string, platform = true, memory = MEMORY, who: Audience = "bench"): string {
  // Filled here, not at module load: `skillIndex` reads the files, and a const above them would run
  // before they are declared.
  // Only the skills that are actually readable: telling a model about a skill its image does not
  // ship is telling it to call something that answers "no skill" (owner, 2026-09-17).
  const index = skillIndex();
  const whole = platformFor(who);
  const platformText = index.length
    ? whole.replace("%SKILLS%", index.map((s) => `- ${s.name} — ${s.description}`).join("\n"))
    : whole.split("\n").filter((l) => !l.includes("%SKILLS%") && !l.includes("load its skill")).join("\n");
  // The memory is the person's, so it rides in every session — including a fork, which has no
  // tools but may well be asked what the person prefers.
  return [hands, ...(platform ? [platformText] : []), ...(memory ? [`What you already know about this person:\n\n${memory}`] : []), ...CAVEMAN].join("\n\n");
}

/**
 * The memory index, read once at load from beside the session files. A workspace session reads the
 * same file: it is the person's memory, and the bench folder travels with their machine.
 */
function memoryIndex(): string {
  const dir = process.env.KL_BENCH_DIR || (process.env.KL_WORKSPACE ? path.join(process.env.KL_WORKSPACE, ".bench") : "");
  if (!dir) return "";
  try {
    return fs.readFileSync(path.join(dir, "memory", "MEMORY.md"), "utf8").trim();
  } catch {
    return "";
  }
}
const MEMORY = memoryIndex();

/**
 * What the model must know, and nothing else. Everything operational — how a proposal is answered,
 * how a result is drawn, how an exchange is tagged — is MECHANISM: it happens whether the model
 * knows about it or not, and describing it here only crowds out the five things it does need
 * (owner, 2026-09-17: "keep the skills simple").
 */
const SHARED = [
  "You have workspaces, environments, snapshots, repos and images. Each has a skill saying what it is and the verbs it has:",
  "%SKILLS%",
  "Before acting in one of these areas, load its skill with `skill {name}` once per session, then tool_search the verb.",
  "You start with ask, plan, skill, tool_search, memory, architecture and question. Every platform tool is one `tool_search` away: search it by what you want to do, and it turns on.",
  "Ask a workspace for information with kind: info — it answers from a read-only copy without stopping its work. Ask for work with kind: work.",
  "An ask you are already waiting on WAKES you when it answers. Do not poll it, and never start, stop or restart a RUNNING machine to move work along — it is already running. A machine that is STOPPED is the exception: start it, because nothing can happen on it until somebody does.",
  "",
  "Independent work that does not need your context goes to an agent with a precise brief; keep its conclusion, not its transcript. Run agents in parallel when tasks are independent. Each works in its own copy of the workspace's working directory and leaves a branch or a pull request behind; its copy and its transcript stay until you close it with `ask_close`.",
  "More than one step? The plan tool is the FIRST call, before any other. Mark each item doing then done as you go, and anything you push to later as later with the reason. The person reads the plan, not your text.",
  "",
  "An environment is chosen for the whole SPACE (the team), never for one workspace: every workspace in the space resolves that environment's services by bare name. So \"attach this workspace to that environment\" is kl_env_switch; there is no per-workspace attach to look for.",
  "A workspace is asked, not touched: `ask {to: \"<workspace>\", task}`. Something new (a backend, a service, a project) gets a new workspace.",
  "Packages are nixpkgs attributes, not language names — rustc and cargo, nodejs_22, go, python3, bun, jdk21, gcc; when unsure, load the workspaces skill and use the ones it names.",
  "Never mention hosts, URLs, routes, ports, status codes, commands you ran or where you run — not even when reporting a failure. Say what you could not do for the person and what you need from them.",
  "Never ask a question to confirm an action. Call the tool; the harness asks the person for you, with what the tool is about to do. Use question ONLY when they must choose between real alternatives you cannot decide.",
  "When the person corrects you, states a preference, or tells you a fact about their setup you will need again, save a memory. Never save what a tool can answer, and never save a conclusion about the harness's own behaviour — report that instead.",
  "A line in square brackets that is not an ask — `[task … finished]`, `[task … expired]`, `[watch …]`, `[harness] …` — is a notice from the harness about something you were waiting on. Read it; it needs no reply and nothing to be started again.",
  "Independent commands go in one turn, together; they run at the same time.",
  "Do what is asked, directly. No checks first. If it fails, say the error in one line.",
  "Only the tools reach the platform. Never change anything the person did not ask for.",
  "Answer in one line, then only the facts needed — eight lines at most, no code blocks and no tables. The tool result is already on screen; never repeat its fields. A thing you changed but could not verify is \"changed, unverified\" — never a claim that it works.",
];

/** True only where the session has no hands: everything it wants done happens in a workspace. */
const BENCH_ONLY = [
  "You have no files and no shell here. Anything that reads, writes or runs happens in a WORKSPACE, through a session that has hands there: ask it.",
  "You do not read code. Ask the workspace; its reply tells you what changed and where.",
  "An ask carries the person's words, not your paraphrase.",
  "An agent started from here works in a workspace you name (`workspace:`); there is no machine here for it.",
  "A package is installed in a workspace, never \"on the bench\": name the workspace.",
  "A `blocked` or `needs the person` reply is a question for the person: put it to them with `question` (or say it in one line if it is not a choice), then re-ask the same workspace with their answer. A workspace that is stopped is started with its start verb — that is not moving work along, it is the person's machine being off.",
];

/** True only where the session IS the machine. */
const WORKSPACE_ONLY = [
  "This machine is yours: \"install X\" or \"switch environment\" means here.",
];

/** One block per audience: a line that is false for the reader is worse than a line it is missing. */
function platformFor(who: Audience): string {
  return [...SHARED, ...(who === "bench" ? BENCH_ONLY : WORKSPACE_ONLY)].join("\n");
}

/** pi's `before_agent_start` hook hands back the system prompt for the turn; returning our own replaces it. */
export function tellItWhereItStands(pi: ExtensionAPI, hands: string, platform = true, who: Audience = "bench"): void {
  const prompt = identity(hands, platform, MEMORY, who);
  pi.on("before_agent_start", async () => ({ systemPrompt: prompt }));
}

/** The opening line: which machine this session is. Everything after it is the same for both. */
export const BENCH_HANDS = [
  "You are the Kloudlite harness, the person's bench.",
  // Spec §3.1 and §3.5, verbatim: the bench session has no hands and no directory at all.
  "You have no filesystem or shell where you run. Every read, edit and command is a tool call that names a workspace and a tree.",
  "You have no working directory. Name a workspace.",
  // §3.5's own paragraph, which was in the workspace identity and not in this one: 53 assistant
  // rows named a host, a port or a container path to the person (transcripts, 2026-09-18).
  "Every path you give or receive is relative to a working directory. Do not explore, describe or depend on where that directory sits on a machine, what is beside it, or how the machine is laid out. Never repeat a path a tool printed that starts with a slash.",
].join("\n");


/**
 * Registering one tool from the catalogue: the description a person reads is the
 * description the model gets, and a call that hands work to a workspace or an
 * environment is published as an exchange, which is what the workspace's own
 * queue and the session's queue both read.
 */
export function makeReg(pi: ExtensionAPI) {
  const spec = (name: string) => TOOLS.find((t) => t.name === name)!;
  const names: string[] = [];
  /**
   * Argument combinations a tool refuses before the card is drawn. A call that
   * cannot be executed must not ask a person to approve it; the handler keeps
   * the same check, so a call that reaches it is still refused.
   */
  const inspect: Record<string, (args: Record<string, JsonValue>) => { code: OperationErrorCode; reason: string } | undefined> = {
    kl_workspace_create: (args) => {
      const issues = workspaceCreateSourceIssues(args);
      return issues.length ? { code: "invalid_args", reason: issues.map((issue) => issue.message).join("; ") } : undefined;
    },
    kl_intercept: (args) => Object.prototype.hasOwnProperty.call(args, "workspace") ? undefined : { code: "invalid_args", reason: "workspace must name a target or be explicit null to clear the intercept" },
  };
  const reg = <P extends Parameters<typeof Type.Object>[0]>(name: string, params: P, run: (a: Record<string, any>, signal?: AbortSignal, ctx?: any) => Promise<{ content: { type: "text"; text: string }[]; isError?: boolean }>) => {
    const s = spec(name);
    names.push(name);
    pi.registerTool({
      name,
      label: name,
      description: `${s.summary} [${s.effect}]`,
      parameters: Type.Object(params),
      async execute(toolCallId, a, signal, _update, ctx) {
        const args = a as Record<string, any>;
        // An EXCHANGE is one session handing work to another — an ask, an info ask, an agent.
        // A platform call is not one: publishing every `kl_workspace_create` here put
        // `kl_workspace_create {"name":"backend-rust","packages":["rust"]}` in the queue as though
        // somebody were waiting on it (owner, 2026-09-17). The call renders as its own tool row.
        // A tool that throws — a name two things answer to, a repo that is not owner/name — answers
        // with the sentence, not with a stack. The sentence is cleaned the same way a failed call's
        // is: a thrown error can carry a URL too (owner, 2026-09-18).
        // A SYNCHRONOUS throw — an argument refused while the call is still being built — escaped
        // this catch and surfaced as a stack (owner, 2026-09-18); both kinds answer the sentence.
        //
        // Changing somebody's platform is asked first (owner, 2026-09-17): the desktop draws the
        // question, the person answers it, and only then does this run. A message to another
        // session and this machine's own packages are not that, and are never gated. The gate and
        // the handler call are one adapter, shared with the operation executor.
        const outcome = await dispatchWithPolicy({
          capability: name,
          effect: s.effect,
          approval: gated(name) ? { required: "user", obtain: () => propose(`p-${toolCallId}`, name, args, ctx, signal) } : { required: "none" },
          inspect: () => inspect[name]?.(args as Record<string, JsonValue>),
          run: () => run(args, signal, ctx),
          error: (e) => ({ ...text(thrown(name, e as Error)), isError: true }),
        });
        // pi's `AgentToolResult` carries a required `details` slot; these tools have no structured
        // details to add, and a JSON envelope drops an undefined one, so nothing else changes.
        return { ...toolResultOf(outcome, "plain"), details: undefined };
      },
    });
  };
  // What this session can do, for `kl_capabilities` to read back: a bench session and a workspace
  // session register different sets, and answering with the other's would be a list of lies.
  return Object.assign(reg, { names });
}

/**
 * The answer to "what can you do here?" — which a model with no matching tool otherwise goes
 * looking for, in the extension's own source or in the `kl` binary (the fleet, 2026-09-17).
 * Registered last, so it names everything else this session holds.
 */
export function capabilities(reg: ReturnType<typeof makeReg>) {
  reg("kl_capabilities", {}, async () => {
    const mine = TOOLS.filter((t) => reg.names.includes(t.name));
    const groups = ["workspace", "environment", "platform"] as const;
    const lines = groups.flatMap((g) => {
      const rows = mine.filter((t) => t.group === g);
      return rows.length ? [`${g}:`, ...rows.map((t) => `  ${t.name} [${t.effect}] — ${t.summary}`)] : [];
    });
    // What this session actually holds. A bench session has no files and no shell of its own; a
    // workspace session's hands reach ITS workspace over the tool interface, never this container.
    const hands = reg.names.some((n) => n === "read" || n === "bash")
      ? ["your workspace, through the tool interface (paths are relative to your working directory):", "  read, write, edit, bash (background: true for a long-running one), process, grep, find, ls [write where they change a file]"]
      : ["you have no files and no shell of your own: name a workspace, and its session does the work."];
    return text([...hands, ...lines, "anything not listed is not something you can do — say so."].join("\n"));
  });
}

/**
 * Ask before changing anything. The proposal is published on the one channel an extension has to
 * the harness (`setWidget`), and the answer comes back through the bench, which holds the question
 * until a person answers it in the desktop. Nothing is guessed: no answer inside the cap is a NO,
 * and so is a bench that cannot be reached — a change nobody agreed to must not happen because a
 * socket dropped.
 */
const PROPOSAL_CAP_MS = 10 * 60_000;
export async function propose(
  id: string,
  tool: string,
  args: Record<string, any>,
  ctx: { ui?: { setWidget?: (k: string, lines: string[]) => void } } | undefined,
  signal?: AbortSignal,
  /** The one line the person reads, when the catalogue has no `ask` for this tool. */
  summary?: string,
  /** What the card shows under that line: a diff, a file's first lines, the command. */
  preview?: string,
): Promise<boolean> {
  ctx?.ui?.setWidget?.("harness:proposal", [JSON.stringify({ id, tool, args, summary: summary ?? question(tool, args), preview })]);
  try {
    const r = await fetch(`${BENCH_URL()}/proposals/${encodeURIComponent(id)}/wait?cap=${PROPOSAL_CAP_MS}${process.env.KL_SESSION ? `&session=${encodeURIComponent(process.env.KL_SESSION)}` : ""}`, { signal });
    return r.ok && ((await r.json()) as { answer?: string }).answer === "yes";
  } catch {
    return false;
  }
}


/** Where harness-bench listens for its own extension; a test points this elsewhere. */
const BENCH_URL = () => process.env.KL_BENCH_URL ?? "http://127.0.0.1:7789";
const benchCall = async (method: string, p: string, body?: unknown): Promise<{ ok: boolean; data: any }> => {
  const r = await fetch(`${BENCH_URL()}${p}`, { method, headers: { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
  return { ok: r.ok, data: await r.json().catch(() => ({ error: `the bench answered ${r.status}` })) };
};

/**
 * The twelve tools a session starts with, and the rest one search away. 43 tools in front of a
 * model is a menu it reads instead of working (owner, 2026-09-17: "43 tools is huge"); every
 * platform tool stays REGISTERED — the proposals, the cards and the waits are unchanged — but
 * inactive until `tool_search` finds it, which is also how a model learns the name it needs.
 */
export const ALWAYS_ON = ["read", "write", "edit", "bash", "grep", "find", "ls", "process", "ask", "ask_close", "plan", "skill", "tool_search", "memory", "architecture", "question"];

/**
 * What a BENCH session starts with. No `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls` or
 * `process`: it has no filesystem and no shell where it runs (spec §3.1), and those names do not
 * exist in that mode at all — a name that answers "not found" teaches a model to try again.
 */
export const BENCH_ALWAYS_ON = ["ask", "ask_close", "plan", "skill", "tool_search", "memory", "architecture", "question"];

/** What Plan mode leaves on: everything that reads, plus the plan itself. */
export const PLAN_TOOLS = ["read", "grep", "find", "ls", "plan", "skill", "tool_search", "memory", "architecture", "kl_capabilities", "kl_workspace_progress"];

/** The six skills, read from beside the extension: product words, not tool lists. */
export const SKILLS = ["workspaces", "environments", "snapshots", "repos", "images", "agents"];
/** Where the skills live beside the extension. Named in the error, because a missing directory in
 *  an image is the usual reason a skill "does not exist" (owner, 2026-09-17). */
// @ts-expect-error See caveman(): pi loads this source as ESM outside the desktop CommonJS build.
export const skillDir = () => path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "skills");

const sharedAdapters = () => createSharedAdapters({ platform: call, bench: benchCall, ownBench: process.env.KL_WORKSPACE_ID });
const shared = async (capability: string, args: Record<string, JsonValue>, states: ArgumentStates = {}) => {
  const adapter = sharedAdapters()[capability];
  if (!adapter) return { ...text(`no trusted adapter for ${capability}`), isError: true };
  return adapterToolResult(await adapter({ args, states }));
};
/**
 * A skill is asked for the way a person says it: `workspaces`, `Workspaces`, ` workspaces `, or the
 * file's own `workspaces.md`. Ten calls in the transcripts were answered `no skill workspaces;
 * there are workspaces, …` — a refusal that names the thing it is refusing (2026-09-18).
 */
export const skillName = (raw: string): string | undefined => {
  const want = String(raw).trim().toLowerCase().replace(/\.md$/, "");
  return SKILLS.find((n) => n === want);
};
function skillText(raw: string): string | undefined {
  const name = skillName(raw);
  if (!name) return undefined;
  try {
    return fs.readFileSync(path.join(skillDir(), `${name}.md`), "utf8");
  } catch {
    return undefined;
  }
}

/**
 * When to load each one. A list of names is a list a model skips: what makes a skill get used is
 * the sentence saying what it is FOR, in the prompt, where it is read before anything is decided
 * (owner, 2026-09-17 — Claude Code's own shape). Read from each file's frontmatter, so the file
 * and the prompt cannot drift.
 */
export const skillIndex = (): { name: string; description: string }[] =>
  SKILLS.map((name) => ({ name, description: /^description:\s*(.*)$/m.exec(skillText(name) ?? "")?.[1]?.trim() ?? "" })).filter((x) => x.description);

/** One line per match, as the catalogue describes it: what it is called, what it does, what it takes. */
function describeTool(pi: ExtensionAPI, name: string): string {
  const t = TOOLS.find((x) => x.name === name);
  const registered = (pi.getAllTools?.() ?? []).find((x: { name: string }) => x.name === name) as { parameters?: { properties?: Record<string, unknown> } } | undefined;
  const params = Object.keys(registered?.parameters?.properties ?? {});
  return `${name} — ${t?.summary ?? ""} [${t?.effect ?? "read"}]${params.length ? `; params: ${params.join(", ")}` : ""}`;
}

/**
 * `skill` and `tool_search`: the two ways out of the twelve. A search that finds nothing says so
 * in the words the model should then use with the person, rather than leaving it to invent a tool.
 */
/**
 * What the person told us, kept for every session afterwards. The bench owns the files — a
 * workspace session has no bench filesystem — so this is one call through the same door.
 */
/**
 * Asking the PERSON. Not every unknown is a thing to guess at: when two ways forward are both
 * reasonable and only they can choose, the question is the work. It rides the proposal channel —
 * the desktop already draws a card and holds the tool until somebody answers — and the answer
 * comes back both as the tool's result and as their own row in the transcript.
 */
/** A question that is really a confirmation: the harness asks for those itself. */
const CONFIRMING = /^\s*(confirm|do you want|proceed|are you sure|shall i|should i)\b/i;

/**
 * What a WORKSPACE session may read of the platform: the lists and the one-record reads, never a
 * write on somebody else's machine. Its own machine's writes are `ownTools`.
 */
export function lookAround(reg: ReturnType<typeof makeReg>) {
  const S = (d: string) => Type.String({ description: d });
  const O = <T>(t: T) => Type.Optional(t as any);
  const q = (o: Record<string, string | undefined>) => {
    const p = Object.entries(o).filter(([, v]) => v).map(([k, v]) => `${k}=${encodeURIComponent(v!)}`);
    return p.length ? `?${p.join("&")}` : "";
  };
  reg("kl_workspaces", { team: O(S("team slug; absent = personal")) }, (a) => shared("workspace.list", a));
  reg("kl_workspace", { id: S("workspace id or name") }, (a) => shared("workspace.inspect", a));
  reg("kl_workspace_snapshots", { id: S("workspace id") }, (a) => answer("GET", `/v1/workspaces/${a.id}/snapshots`));
  reg("kl_environments", { team: O(S("team slug; absent = personal")) }, (a) => answer("GET", `/v1/environments${q({ team: a.team })}`));
  reg("kl_environment", { id: S("environment id") }, (a) => answer("GET", `/v1/environments/${a.id}`));
}

export function questionTool(reg: ReturnType<typeof makeReg>, pi?: ExtensionAPI) {
  reg(
    "question",
    {
      header: Type.String({ description: "two or three words: what this is about" }),
      question: Type.String({ description: "the question itself, in one sentence" }),
      options: Type.Array(Type.Object({ label: Type.String(), description: Type.String({ description: "one line: what choosing it means" }) }), { description: "two to four ways forward" }),
      multi: Type.Optional(Type.Boolean({ description: "more than one may be chosen" })),
    },
    async (a, signal, ctx) => {
      // A CONFIRMATION is not a question: every tool that changes something already asks the person
      // through the harness, so a hand-made "Do you want me to…" is one prompt too many — and it
      // arrives without the tool's own arguments to judge it by (owner, 2026-09-17).
      const confirming = CONFIRMING.test(a.header ?? "") || CONFIRMING.test(a.question ?? "");
      // Whether the tool that would ask is armed YET is not the point: a confirmation is one prompt
      // too many either way, and half the questions in the transcripts were confirmations of a tool
      // that simply had not been searched for (2026-09-18).
      if (confirming)
        return { ...text("not needed: call the tool, the harness will ask the person for you. Use question only when they must choose between real alternatives you cannot decide."), isError: true };
      const id = `q-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
      ctx?.ui?.setWidget?.("harness:proposal", [JSON.stringify({ id, tool: "question", args: a, summary: a.question, question: { header: a.header, options: a.options, multi: a.multi } })]);
      const r = await fetch(`${BENCH_URL()}/proposals/${encodeURIComponent(id)}/wait?cap=${PROPOSAL_CAP_MS}${process.env.KL_SESSION ? `&session=${encodeURIComponent(process.env.KL_SESSION)}` : ""}`, { signal }).catch(() => undefined);
      const answer = r?.ok ? ((await r.json()) as { answer?: string }).answer : undefined;
      // No answer is an answer: it says stop and ask them properly, not pick one and carry on.
      return answer && answer !== "no" ? text(answer) : { ...text("the person did not answer; ask them in your reply instead of choosing"), isError: true };
    },
  );
}

export function memoryTools(reg: ReturnType<typeof makeReg>) {
  reg(
    "memory",
    {
      save: Type.Optional(Type.Object({
        name: Type.String({ description: "lowercase words with dashes, e.g. deploys-from-the-pod" }),
        description: Type.String({ description: "one line; this is what you will see in the index later" }),
        type: Type.String({ description: "user (a preference), feedback (a correction), project (how their work is set up), reference (a fact)" }),
        body: Type.String({ description: "the memory itself; for feedback and project, **Why:** … **How to apply:** …" }),
      })),
      forget: Type.Optional(Type.String({ description: "the name of a memory that is no longer true" })),
    },
    async (a) => {
      if (a.forget) {
        const r = await benchCall("DELETE", `/memory/${encodeURIComponent(a.forget)}`);
        return r.ok ? text(`forgot ${a.forget}`) : { ...text(String(r.data?.error ?? "no such memory")), isError: true };
      }
      if (!a.save) return { ...text("memory takes save (name, description, type, body) or forget (a name)"), isError: true };
      const r = await benchCall("POST", "/memory", a.save);
      return r.ok ? text(`saved ${a.save.name}`) : { ...text(String(r.data?.error ?? "it was not saved")), isError: true };
    },
  );
}

/**
 * The space's architecture (§24): what runs where, what talks to what, and on which endpoints. The
 * bench holds ONE document for the space and every session reads and writes it, because the
 * question "which port does the api answer on" has one answer and it is not worth asking a
 * workspace for.
 *
 * With no arguments it reads. With `set` it replaces one `##` section, so a workspace updates what
 * it owns without touching anybody else's.
 */
export function architectureTools(reg: ReturnType<typeof makeReg>) {
  reg(
    "architecture",
    {
      set: Type.Optional(Type.Object({
        section: Type.String({ description: "the `##` heading to replace or add — a component, a service, or `Contracts`" }),
        text: Type.String({ description: "what that section says now, in markdown; contract level, never implementation detail" }),
      })),
    },
    async (a) => {
      if (!a.set) {
        const r = await benchCall("GET", "/architecture");
        return r.ok ? text(String(r.data?.text ?? "").trim() || "the architecture document is empty") : { ...text(String(r.data?.error ?? "it could not be read")), isError: true };
      }
      const r = await benchCall("PUT", "/architecture", a.set);
      return r.ok ? text(`architecture: ${a.set.section} updated`) : { ...text(String(r.data?.error ?? "it was not written")), isError: true };
    },
  );
}

/**
 * Build ⇄ Plan, as a command the desktop sends (`/mode plan`). pi's RPC has no "set active tools",
 * and it should not: which tools a session may call is the EXTENSION's business, and this is the
 * extension turning its own writes off. Plan mode is read-only plus `plan`, so a plan cannot
 * quietly become a change.
 */
export function modeCommand(pi: ExtensionAPI, planTools: string[]) {
  pi.registerCommand?.("mode", {
    description: "build (everything) or plan (read-only, plus the plan tool)",
    handler: async (args, ctx) => {
      const plan = String(args ?? "").trim().toLowerCase() === "plan";
      pi.setActiveTools?.(plan ? planTools.filter((n) => (pi.getAllTools?.() ?? []).some((t: { name: string }) => t.name === n)) : ALWAYS_ON);
      ctx.ui?.notify?.(plan ? "plan mode: nothing changes until you switch back" : "build mode", "info");
    },
  });
}

export function searchTools(reg: ReturnType<typeof makeReg>, pi: ExtensionAPI) {
  reg("skill", { name: Type.Optional(Type.String({ description: `${SKILLS.join(", ")}; absent lists them` })) }, async (a) => {
    const result = await shared("skill.read", a);
    if (a.name || result.isError) return result;
    const rows = JSON.parse(result.content[0].text) as { name: string; description: string }[];
    return text(rows.map((row) => `${row.name} — ${row.description}`).join("\n"));
  });
  reg("tool_search", { query: Type.String({ description: "what you want to do, in a word or two" }) }, async (a) => {
    const words = String(a.query).toLowerCase().split(/[^a-z0-9_]+/).filter((w) => w.length > 2);
    const hit = TOOLS.filter((t) => {
      const hay = `${t.name} ${t.summary} ${t.group}`.toLowerCase();
      return words.length ? words.every((w) => hay.includes(w)) || words.some((w) => t.name.includes(w)) : false;
    });
    // ONLY what this session actually has. The catalogue is the whole platform; a workspace session
    // registers a part of it, and offering a name pi never registered answered "Tool not found" on
    // every call (owner, 2026-09-17).
    const registered = new Set((pi.getAllTools?.() ?? []).map((t: { name: string } | string) => (typeof t === "string" ? t : t.name)));
    const here = registered.size ? hit.filter((t) => registered.has(t.name)) : hit;
    if (!here.length) return text("no tool for that here; say so to the person, and do not search again for the same thing");
    // Activated for the rest of the SESSION, which outlives this process: the bench restarted under
    // the owner and the next turn answered `tool kl_workspace_create not found` for a tool the model
    // had already called (owner, 2026-09-18). The names are recorded on the bench, which is what a
    // starting session arms itself from.
    const active = pi.getActiveTools?.() ?? ALWAYS_ON;
    const names = here.map((t) => t.name);
    pi.setActiveTools?.([...new Set([...active, ...names])]);
    void rememberFound(names);
    // A search hit lasts the SESSION. Without being told so the model searched the same verb
    // again every turn — "list workspaces" six times across six sessions (transcripts, 2026-09-18).
    return text([...here.map((t) => describeTool(pi, t.name)), "these are on for the rest of this session; call them, do not search for them again"].join("\n"));
  });
}

/**
 * Agents and the plan — Claude Code's own two shapes (owner, 2026-09-17). An AGENT is a fresh
 * session in a workspace with one task and no history, running in the background and reporting
 * once; it is the ask machinery pointed at a session that did not exist a moment ago. The PLAN is
 * what this session says it will do, drawn in the inspector.
 *
 * An agent cannot start agents: its own child would have nobody to report to and no way to be
 * seen. `KL_EPHEMERAL` is how a session knows it is one.
 */
export function agentTools(reg: ReturnType<typeof makeReg>, own: string | undefined) {
  const slug = (s: string) => (s || "agent").toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 24) || "agent";
  reg(
    "ask",
    {
      to: Type.String({ description: 'a workspace, a LIVE agent\'s name (which resumes it, with everything it has done), or "agent" for a fresh one' }),
      task: Type.String({ description: "the person's request in their own words, one line; add nothing, rewrite nothing" }),
      brief: Type.Optional(Type.String({ description: "only what the receiver cannot know: constraints, what to answer with. It cannot see this conversation." })),
      name: Type.Optional(Type.String({ description: 'what to call the agent; only with to: "agent"' })),
      model: Type.Optional(Type.String({ description: "a model for this agent; absent = the session's own" })),
      workspace: Type.Optional(Type.String({ description: "where the agent works; the bench must name one, a workspace session may omit it for its own" })),
      kind: Type.Optional(Type.String({ description: 'work (default: it does something) or info (a question about the workspace\'s code or state that changes nothing)' })),
    },
    async (a) => {
      // One verb, two shapes: a workspace REMEMBERS (its own session, a teammate), an agent starts
      // clean and is thrown away. A person says "ask X to…" for both, so the tool is one.
      if (a.to === "agent") {
        const name = `${slug(a.name ?? a.task.split(/\s+/).slice(0, 3).join("-"))}-${Math.random().toString(36).slice(2, 8)}`;
        // Only a workspace session may leave it out: `own` on the bench is the BENCH's id, so the
        // default sent the agent to a machine that is not a workspace at all. Refused in the same
        // words `kl_pkg_add` uses, and before anything is started.
        const where = a.workspace ?? (process.env.KL_TOOLS_WORKSPACE ? own : undefined);
        if (!where) return { ...text("name the workspace: an agent works in a workspace, and this session has no machine of its own"), isError: true };
        // ISOLATED, and no second machine: the bench asks the workspace for a TREE of itself — a
        // nested snapshot inside the same pod — so two agents changing files at once cannot trip
        // over each other, a refactor that goes wrong is thrown away with the tree, and the caches
        // are already warm (spec §4.1). The bench cuts it and waits; there is no session until
        // there is somewhere to work.
        const task = [a.task, a.brief].filter(Boolean).join("\n\n");
        const r = await benchCall("POST", "/agents", { task, workspace: where, name, model: a.model, from: process.env.KL_SESSION });
        if (!r.ok) return { ...text(String(r.data?.error ?? "the bench could not start it")), isError: true };
        return text(`agent ${name} started`);
      }
      // A live agent by name RESUMES it — "one more thing", "fix round" — with everything it has
      // done still in front of it; the bench routes on the name, so one verb covers both.
      const kind = a.kind === "info" ? "info" : "work";
      const r = await benchCall("POST", `/workspaces/${encodeURIComponent(String(a.to))}/ask`, { text: [a.task, a.brief].filter(Boolean).join("\n\n"), kind, from: process.env.KL_SESSION });
      if (!r.ok) return { ...text(String(r.data?.error ?? `the bench answered about ${a.to}`)), isError: true };
      return text(kind === "info" ? `asked ${a.to}; it answers from a read-only copy without stopping` : `queued in ${a.to}'s session; its reply arrives here`);
    },
  );
  // Closing is a PROPOSAL like every other state change (spec §4.3): it is gated in the catalogue,
  // so the card, the wait and the person's yes are `makeReg`'s, not a second path here.
  reg("ask_close", { name: Type.String({ description: "the agent's name" }) }, async (a) => {
    const r = await benchCall("DELETE", `/agents/${encodeURIComponent(a.name)}`);
    if (!r.ok) return { ...text(String(r.data?.error ?? "no such agent")), isError: true };
    return text(`agent ${a.name} closed`);
  });
}

/**
 * Images, from a session with no machine. Listing the registry is a READ of the platform and needs
 * nobody's tree; building one needs a tree, so the bench asks the workspace that has it — the
 * workspace session runs `kl container build` where the context actually is. The bench proposed
 * starting ITSELF to get a builder, because its image tools defaulted to "this machine" and the
 * bench has none (owner, 2026-09-18).
 */
export function imageTools(reg: ReturnType<typeof makeReg>) {
  const S = (d: string) => Type.String({ description: d });
  reg("kl_images", { owner: Type.Optional(S("owner slug; absent = your own")) }, async (a) =>
    answer("GET", `/api/${encodeURIComponent(String(a.owner ?? process.env.KL_OWNER ?? process.env.KL_TEAM ?? "me"))}/images`, undefined, "kl_images"));
  const build = async (workspace: string, what: string) => {
    refuseOwnBench(workspace);
    const r = await benchCall("POST", `/workspaces/${encodeURIComponent(workspace)}/ask`, { text: what, kind: "work", from: process.env.KL_SESSION });
    if (!r.ok) return { ...text(String(r.data?.error ?? "the workspace could not be asked")), isError: true };
    return text(`${workspace} was asked to do it; read kl_workspace_progress for how it is going`);
  };
  reg(
    "kl_container_build",
    {
      workspace: S("the workspace holding the build context — required: this session has no machine of its own"),
      context: S("the build context directory, relative to that workspace"),
      tag: S("name:tag; it is pushed under your own owner"),
      dockerfile: Type.Optional(S("a Dockerfile other than the context's own")),
    },
    (a) => build(String(a.workspace), `build and push ${a.tag} from ${a.context}${a.dockerfile ? ` with ${a.dockerfile}` : ""}, with kl_container_build, and tell me the tag when it is pushed`),
  );
  reg(
    "kl_container_push",
    { workspace: S("a workspace to run it in — required: this session has no machine of its own"), from: S("the tag the registry already holds"), to: S("the new tag") },
    (a) => build(String(a.workspace), `copy image ${a.from} to ${a.to} with kl_container_push, and tell me when it is done`),
  );
}

/**
 * Reporting on an ask this session is holding (spec §3.8). An ask is a small conversation: the
 * first report is the DECISION — "going ahead with: add GET /version, bump to 0.1.0, build, push" —
 * which reaches the asking session as one line and answers nothing; `done` or `blocked` answers it.
 * The owner asked for exactly this shape so a bench is not left guessing between "queued" and a
 * finished build (2026-09-18).
 */
export function reportTool(reg: ReturnType<typeof makeReg>) {
  reg(
    "report",
    {
      ask: Type.Optional(Type.String({ description: "the ask id from its `[ask <id> …]` tag; absent when only one is open" })),
      kind: Type.String({ description: "progress (your decision, or a milestone — it does not answer the ask), done, or blocked" }),
      text: Type.String({ description: "one fragment: what you are going ahead with, or the outcome. No files, paths, commands or digests." }),
    },
    async (a) => {
      const kind = a.kind === "done" || a.kind === "blocked" ? a.kind : "progress";
      const r = await benchCall("POST", "/reports", { from: process.env.KL_SESSION, ask: a.ask ?? "", kind, text: a.text });
      if (!r.ok) return { ...text(String(r.data?.error ?? "the report did not reach the asking session")), isError: true };
      return text(kind === "progress" ? `told them: ${a.text}` : `${kind}: the ask is answered`);
    },
  );
}

/** The plan this session is working to: written once, ticked as it lands. */
export function planTools(reg: ReturnType<typeof makeReg>) {
  const publish = (ctx: any, v: unknown) => ctx?.ui?.setWidget?.("harness:plan", [JSON.stringify(v)]);
  const ITEM = Type.Object({ text: Type.String({ minLength: 3, description: "the step, as the person would read it" }), state: Type.Optional(Type.String({ description: "todo, doing, done or later" })), why: Type.Optional(Type.String({ description: "with later: why it is not now" })) });
  reg(
    "plan",
    {
      set: Type.Optional(Type.Array(ITEM, { description: "the whole plan, in order — replaces it" })),
      doing: Type.Optional(Type.String({ description: "the step being worked on now" })),
      done: Type.Optional(Type.String({ description: "the step that just landed" })),
      later: Type.Optional(Type.Object({ text: Type.String(), why: Type.String({ description: "why it is not being done now" }) }, { description: "a step pushed to later, as {text, why} — not a sentence" })),
    },
    async (a, _signal, ctx) => {
      // The person reads the PLAN panel, not a paragraph about the plan: one call keeps it current.
      for (const [k, v] of [["done", a.done], ["doing", a.doing]] as const) {
        if (v === undefined) continue;
        // An empty string published an empty step and the panel showed a blank row (transcripts).
        if (!String(v).trim()) return { ...text(`${k} needs the step's text, the same words the plan has`), isError: true };
        return publish(ctx, { [k]: v }), text(`${k}: ${v}`);
      }
      if (a.later) return publish(ctx, { later: a.later }), text(`later: ${a.later.text} (${a.later.why})`);
      if (!a.set?.length) return { ...text("plan takes set (the steps), doing, done, or later"), isError: true };
      publish(ctx, { set: a.set });
      return text(`plan: ${a.set.length} steps`);
    },
  );
}

/**
 * How a workspace is getting on, without going to look. A model with no tool for this grepped the
 * bench's own `.bench/workspaces/*\/thread.jsonl` off disk (the fleet, 2026-09-17) — refused now,
 * so here is the answer instead: what has been asked of it and what its session has been saying.
 * Both modes have it: a workspace session may have asked something of another one too.
 */
export function progressTool(reg: ReturnType<typeof makeReg>) {
  // `id`, like every other kl_workspace* tool: two keys for the same thing had the model calling
  // `kl_workspace {"workspace": …}` twenty times (transcripts, 2026-09-18).
  reg("kl_workspace_progress", { id: Type.String({ description: "workspace id or name" }) }, async (a) => {
    // The bench keys a workspace's thread and its exchanges by the workspace ID. Asked by NAME —
    // which is what a person says and what this tool now takes — both reads answered nothing, so a
    // workspace mid-task reported "nothing outstanding / nothing yet" (transcripts, 2026-09-18).
    refuseOwnBench(String(a.id));
    const progress = resolveWorkspaceProgress(call, benchCall, process.env.KL_WORKSPACE_ID);
    let raw = await progress({ args: a, states: {} });
    if (!raw.ok && raw.error.code === "no_match") raw = await sharedAdapters()["workspace.progress"]({ args: a, states: {} });
    const result = adapterToolResult(raw);
    if (result.isError) return result;
    const data = JSON.parse(result.content[0].text) as { asks: any[]; messages: any[]; processes: any[] };
    const x = { ok: true, data: data.asks };
    const m = { ok: true, data: { messages: data.messages } };
    const procs = { ok: true, data: data.processes };
    // ONE line per ask — state, how long, its first line — never the ask's body: the bench holds
    // what things ARE, the workspace holds how they are done (owner, 2026-09-18). And an ask that is
    // running WAKES this session when it answers, so polling it is six calls that learn nothing.
    const asks = (x.data as { dir: string; state: string; text: string; ts?: number }[])
      .filter((e) => e.dir === "out")
      .map((e) => {
        const first = String(e.text).replace(/^\[ask \S+ from [^\]]*\] /, "").split("\n")[0].slice(0, 100);
        const age = e.ts ? ` for ${Math.max(0, Math.round((Date.now() - e.ts) / 1000))}s` : "";
        const wait = e.state === "running" || e.state === "queued" ? "; you will be told when it answers — do not poll" : "";
        return `  ${e.state}${age}: ${first}${wait}`;
      });
    const said = ((m.data as { messages?: Record<string, any>[] }).messages ?? []).slice(-10).flatMap((r) => {
      const c = r.content;
      if (r.role === "user") return [`  asked: ${(typeof c === "string" ? c : (c ?? []).map((b: any) => b.text ?? "").join("")).slice(0, 160)}`];
      if (r.role !== "assistant") return [];
      // A tool call is what it is DOING; the prose is what it thinks about it. Both, briefly.
      return (c as any[] ?? []).map((b) => (b.type === "toolCall" ? `  ran ${b.name}` : b.text ? `  said: ${String(b.text).slice(0, 160)}` : "")).filter(Boolean);
    });
    const ws = String((data.processes as any[])[0]?.workspace ?? a.id);
    const running = (procs.ok && Array.isArray(procs.data) ? (procs.data as { workspace?: string; name: string; command: string; started: number; ended?: number }[]) : [])
      .filter((r) => r.workspace === ws && r.ended === undefined)
      .map((r) => `  ${r.name}: ${String(r.command).slice(0, 120)} (since ${new Date(r.started).toISOString().slice(11, 16)})`);
    return text(
      [
        `asked of ${a.id}:`,
        ...(asks.length ? asks : ["  nothing outstanding"]),
        `running there:`,
        ...(running.length ? running : ["  nothing running"]),
        `its session, latest last:`,
        ...(said.length ? said : ["  nothing yet"]),
      ].join("\n"),
    );
  });
}

/**
 * The machine this session IS: the bench's own workspace, or, in a workspace
 * session, that workspace. Packages are read-modify-write against `/v1` because
 * PATCH takes the WHOLE list — "add nats" has to keep what is already there.
 */
/**
 * An id, or the NAME a person calls it, for the tools registered outside `tools()`. Every other
 * environment tool takes either, and `kl_env_switch` taking only an id is why `{"environment":
 * "devstack"}` answered 404 and had to be retried by id (transcripts, 2026-09-18).
 */
export async function resolveNamed(kind: "workspaces" | "environments", idOrName: string): Promise<string> {
  // No `team` query means "personal only" (`/v1`'s own default); a team bench must list its
  // team's rows too, or every id lookup below sees an empty listing.
  const team = process.env.KL_TEAM;
  const r = await call("GET", `/v1/${kind}${team ? `?team=${encodeURIComponent(team)}` : ""}`);
  const rows = (Array.isArray(r.data) ? r.data : []) as { id?: string; name?: string }[];
  const resolved = resolveUnique(rows as JsonValue[], idOrName, kind.slice(0, -1));
  if (resolved.ok) return resolved.id;
  // Not in the listing: hand it on as given, so /v1's own 404 is the answer rather than ours —
  // this also covers a team workspace this listing didn't resolve.
  if (resolved.code === "no_match") return idOrName;
  throw new Error(resolved.message);
}

/**
 * The SPACE's environment, which belongs to the person and not to any one machine: a bench session
 * keeps these even though it has no machine of its own (spec §3.1 removes hands, not the platform).
 */
export function spaceTools(reg: ReturnType<typeof makeReg>, space: string | undefined) {
  if (!space) return;
  reg("kl_env_current", {}, () => answer("GET", "/v1/me/environments"));
  reg("kl_env_switch", { environment: Type.String({ description: "environment id or name" }) }, async (a) => answer("PUT", `/v1/me/environments/${encodeURIComponent(space)}`, { environment: await resolveNamed("environments", String(a.environment)) }));
  reg("kl_env_clear", {}, () => answer("DELETE", `/v1/me/environments/${encodeURIComponent(space)}`));
}

/**
 * Packages, from a session that has no machine of its own: the WORKSPACE is named, and the change
 * lands on that workspace's spec (spec §3.1, "a package request always names a workspace"). The
 * refusal is the spec's own sentence, because a model that assumed "here" was the defect.
 */
export function packageTools(reg: ReturnType<typeof makeReg>) {
  const P = Type.Array(Type.String(), { description: "nixpkgs ATTRIBUTE names, not language names: rustc cargo (Rust), nodejs_22, go, python3, bun, pnpm, jdk21, gcc, gnumake; `attr@version` pins one" });
  const WS = Type.Optional(Type.String({ description: "the workspace to act on, by name or id — required: this session has no machine of its own" }));
  const NAME_IT = { ...text("name the workspace: packages are installed in a workspace, and this session has no machine of its own"), isError: true };
  const named = (workspace: unknown) => (typeof workspace === "string" && workspace.trim() ? refuseOwnBench(workspace.trim()) : undefined);

  reg("kl_pkg_list", { workspace: WS }, async (a) => {
    const id = named(a.workspace);
    return id ? answer("GET", `/v1/workspaces/${encodeURIComponent(id)}`) : NAME_IT;
  });
  reg("kl_pkg_add", { workspace: WS, packages: P }, async (a) => {
    const id = named(a.workspace);
    if (!id) return NAME_IT;
    return shared("workspace.packages.add", { workspace: id, packages: a.packages });
  });
  reg("kl_pkg_rm", { workspace: WS, packages: P }, async (a) => {
    const id = named(a.workspace);
    if (!id) return NAME_IT;
    return shared("workspace.packages.rm", { workspace: id, packages: a.packages });
  });
}

/**
 * A WORKSPACE session's own machine: its packages, and the space's environment. The bench session
 * does not have this — it has no machine (spec §3.2) — and `kl_pkg_*` with no workspace named is
 * refused rather than guessed at.
 */
export function ownTools(pi: ExtensionAPI, own: string, space: string | undefined, reg = makeReg(pi)) {
  const packages = async (): Promise<string[]> => {
    const { status, data } = await call("GET", `/v1/workspaces/${encodeURIComponent(own)}`);
    // This machine is GONE — deleted under a session that stayed open. `404: not found` read as a
    // blip and was retried four times in one session (transcripts, 2026-09-18); it never changes.
    if (status === 404) throw new Error("this machine no longer exists; nothing here can be read or changed, and asking again will not change that");
    if (status >= 400) throw new Error(`${status}: ${typeof data === "string" ? data : JSON.stringify(data)}`);
    return ((data as { packages?: string[] } | null)?.packages ?? []).slice();
  };
  // A pin is `attr@version`: matching on the attr alone is what lets "remove nodejs" take `nodejs@20`.
  const attr = (e: string) => e.split("@")[0];
  const setPackages = async (next: string[]) => answer("PATCH", `/v1/workspaces/${encodeURIComponent(own)}`, { packages: next });
  const P = Type.Array(Type.String(), { description: "nixpkgs ATTRIBUTE names, not language names: rustc cargo (Rust), nodejs_22, go, python3, bun, pnpm, jdk21, gcc, gnumake; `attr@version` pins one" });
  reg("kl_pkg_list", {}, async () => text(await packages()));
  reg("kl_pkg_add", { packages: P }, async (a) => {
    const have = await packages();
    // A re-pin replaces the entry it pins rather than sitting beside it.
    const next = [...have.filter((e) => !a.packages.some((x: string) => attr(x) === attr(e))), ...a.packages];
    return setPackages(next);
  });
  reg("kl_pkg_rm", { packages: P }, async (a) => {
    const have = await packages();
    const next = have.filter((e) => !a.packages.some((x: string) => attr(x) === attr(e)));
    if (next.length === have.length) return { ...text(`none of ${a.packages.join(", ")} is installed here`), isError: true };
    return setPackages(next);
  });
  if (!space) return;
  // A person's space follows ONE environment; this machine and every workspace in it resolve its services by bare name.
  reg("kl_env_current", {}, () => answer("GET", "/v1/me/environments"));
  reg("kl_env_switch", { environment: Type.String({ description: "environment id or name" }) }, async (a) => answer("PUT", `/v1/me/environments/${encodeURIComponent(space)}`, { environment: await resolveNamed("environments", String(a.environment)) }));
  reg("kl_env_clear", {}, () => answer("DELETE", `/v1/me/environments/${encodeURIComponent(space)}`));
}

/**
 * The space's environment. Every session that lives in a space may manage it — the bench and every
 * workspace session alike (owner, 2026-09-17): the person debugging in a workspace is the person
 * who needs a service added or its traffic pointed at them, and making them walk back to the bench
 * for it is the same "no tool for this" that sent a model reading extension source.
 *
 * Creating, stopping, cloning, restoring and deleting an environment stay with the bench: those
 * are about the space, not about the work in front of one workspace.
 */
/** A service is up, or the environment itself has stopped moving and it never will be. */
const RESTING_ENV = new Set(["stopped", "error", "failed", "deleted"]);
const SERVICE_CAP = 180_000;

/**
 * Code, through `/v1` — the same routes the web reads. Every session has these: a workspace
 * session is where the work happens, and a bench session opens the pull request for it.
 *
 * `repo` is `owner/name` everywhere, because that is what a person calls it and what every other
 * surface prints; the two segments are encoded separately so a name can never walk the path.
 */
function repoTools(reg: ReturnType<typeof makeReg>) {
  const S = (d: string) => Type.String({ description: d });
  const O = <T>(t: T) => Type.Optional(t as any);
  const R = S("repository as owner/name");
  const at = (repo: string, rest = "") => {
    const [owner, name] = String(repo).split("/");
    if (!owner || !name || name.includes("/")) throw new Error(`repository ${repo} is not owner/name`);
    return `/v1/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}${rest}`;
  };
  reg("kl_repos", { owner: O(S("owner slug; absent = everything you can see")) }, (a) => answer("GET", `/v1/repos${q({ owner: a.owner })}`));
  reg(
    "kl_repo_create",
    { name: S("repository name"), owner: O(S("team slug or your handle; absent = your own")), visibility: O(S("public or private; absent = private")), description: O(S("one line")) },
    (a) => answer("POST", "/v1/repos", { owner: a.owner ?? process.env.KL_OWNER, name: a.name, visibility: a.visibility, description: a.description ?? "" }),
  );
  reg("kl_repo_branches", { repo: R }, (a) => answer("GET", at(a.repo, "/branches")));
  reg("kl_pulls", { repo: R, state: O(S("open, merged or closed")), limit: O(Type.Number()) }, (a) => answer("GET", at(a.repo, `/pulls${q({ state: a.state, limit: a.limit ? String(a.limit) : undefined })}`)));
  reg("kl_pull", { repo: R, number: Type.Number({ description: "pull request number" }) }, (a) => answer("GET", at(a.repo, `/pulls/${Number(a.number)}`)));
  // The author is whoever is signed in; the api refuses to take it from the body, so it is not sent.
  reg("kl_pull_create", { repo: R, title: S("what the change is"), head: S("branch with the change"), base: S("branch to merge into"), body: O(S("the description")) }, (a) =>
    answer("POST", at(a.repo, "/pulls"), { title: a.title, head: a.head, base: a.base, body: a.body ?? "" }));
  reg("kl_pull_merge", { repo: R, number: Type.Number(), method: O(S("fast-forward (default), squash, merge or rebase")) }, (a) =>
    answer("POST", at(a.repo, `/pulls/${Number(a.number)}/merge${q({ strategy: a.method })}`)));
  reg("kl_pull_close", { repo: R, number: Type.Number() }, (a) => answer("POST", at(a.repo, `/pulls/${Number(a.number)}/close`)));
}

function environmentTools(reg: ReturnType<typeof makeReg>) {
  const S = (d: string) => Type.String({ description: d });
  const O = <T>(t: T) => Type.Optional(t as any);
  reg("kl_environments", { owner: O(S("owner slug to list for")) }, (a) => answer("GET", `/v1/environments${q({ owner: a.owner })}`));
  reg("kl_environment", { id: S("environment id") }, (a) => answer("GET", `/v1/environments/${a.id}`));
  reg(
    "kl_intercept",
    {
      id: S("environment id"),
      service: S("service name in the environment"),
      workspace: O(Type.Union([S("workspace id to deliver to"), Type.Null({ description: "explicitly clear the intercept" })])),
      // `/v1`'s own shape (`crd::PortMap`): the service's port and the workspace's. The tool
      // declared `{from, to}`, so every call was a 422 naming a field the model could not see, and
      // it guessed three shapes in a row (transcripts, 2026-09-18).
      ports: O(Type.Array(Type.Object({ service: Type.Number({ description: "the port callers already dial on the service" }), workspace: Type.Number({ description: "the port the workspace listens on" }) }), { description: "port remaps, as {service, workspace}; absent forwards every port 1:1" })),
    },
    (a) => shared("environment.intercept", { id: a.id, service: a.service, ...(a.workspace === null ? {} : { workspace: a.workspace }), ...(a.ports === undefined ? {} : { ports: a.ports }) }, { workspace: a.workspace === null ? { kind: "explicitly_clear" } : a.workspace === undefined ? { kind: "unspecified" } : { kind: "known", value: a.workspace } }),
  );
  // PATCH takes the WHOLE list, so both of these read the environment first and pass every service
  // they are not changing through VERBATIM. A tool that took "the list" and rebuilt each row from
  // a narrower schema would silently drop a service's command, env or mounts — the model cannot
  // see what it did not ask for. Removals take their StatefulSet with them; bytes stay on the volume.
  reg("kl_environment_service_add", { id: S("environment id"), service: SERVICE }, async (a, signal) => {
    const one = service(a.service);
    const r = await shared("environment.service.put", { id: a.id, service: one });
    if (r.isError) return r;
    // The service has to actually come up: waiting here is why nobody sleeps in a shell.
    const w = await settle(
      () => call("GET", `/v1/environments/${encodeURIComponent(a.id)}`),
      (d) => (d?.service_status ?? []).some((x: { name: string; ready?: boolean }) => x.name === one.name && x.ready) || RESTING_ENV.has(String(d?.state ?? "")),
      SERVICE_CAP,
      signal,
    );
    return text(w.settled ? w.data : `${one.name} not ready after ${Math.round(w.waitedMs / 1000)}s\n${JSON.stringify(w.data, null, 2)}`);
  });
  reg("kl_environment_service_rm", { id: S("environment id"), name: S("the service to remove") }, async (a) => {
    return shared("environment.service.rm", a);
  });
}

/**
 * The bench is not a workspace, and a bench session must never name itself as one. It proposed
 * `Start workspace bench-505f6b8c9d7b` — itself — while an ask to a real workspace was still
 * running (owner, 2026-09-18). `/v1` hides benches from every listing but still answers a bench id
 * by name, so the refusal belongs here, where the id is read.
 *
 * A bench session no longer defaults ANY tool to its own machine (spec §3.1: it has none). A
 * workspace session still defaults to its own workspace, which is what `ownTools` is.
 */
export const NOT_A_WORKSPACE = "that is you, not a workspace; name a workspace";
export const isOwnBench = (id: string): boolean => {
  const own = process.env.KL_WORKSPACE_ID;
  // The bench's own workspace id, and the `bench-` objects the platform names a bench with
  // (`crd::bench_id`) — a listing never offers one, so anything shaped like one is a mistake.
  return !!id && (id === own || /^bench-[0-9a-f]{8,}$/.test(id));
};
/** Throws the person's own sentence when a bench session names itself. */
export function refuseOwnBench(id: string): string {
  if (isOwnBench(id)) throw new Error(NOT_A_WORKSPACE);
  return id;
}

/**
 * A listing the model reads never contains a bench. `/v1` already drops them
 * (`api::workspaces::visible`), and this is the second fence: a row that leaked from anywhere —
 * an older api, a cached answer — must not become something the model can name.
 */
export function withoutBenches(data: unknown): unknown {
  if (!Array.isArray(data)) return data;
  return data.filter((r) => {
    const row = r as { id?: string; bench?: unknown; kind?: string };
    return !(row?.bench || row?.kind === "bench" || isOwnBench(String(row?.id ?? "")));
  });
}

/** Per-verb ceilings: long enough for a real create on a cold node, short enough to answer. */
const CAP = { create: 180_000, start: 180_000, restore: 180_000, push: 120_000 };
/**
 * A workspace or environment that has stopped moving, in `/v1`'s OWN words (`crd::Phase::as_str`).
 *
 * `ready` is the one a create ends at — the pod is up and the person can work — and it was missing
 * here, so `kl_workspace_create backend` sat polling for 129 s over a workspace `/v1` had called
 * ready with a running pod at ~100 s, and only stopped when the phase happened to move or the cap
 * ran out (owner, 2026-09-18). A state this app does not know is a state it waits out.
 */
const RESTING = new Set(["ready", "running", "stopped", "idle", "unavailable", "error", "failed", "deleted"]);

export function tools(pi: ExtensionAPI) {
  const reg = makeReg(pi);
  const S = (d: string) => Type.String({ description: d });
  const WS = S("workspace, by name or id");
  const ENV = S("environment, by name or id");
  /**
   * An object's snapshots, as a person thinks of them: what they said, and when. The volume behind
   * them is the platform's business — a person takes a snapshot of their workspace, not of a volume —
   * so it is resolved here and never named in the answer.
   */
  const snapshotsOf = async (kind: "workspaces" | "environments", id: string) => {
    const vols = await call("GET", "/v1/volumes");
    const vol = (vols.data as { name?: string; volume?: string }[] | null)?.find((v) => v.name === id)?.volume;
    if (!vol) return text("no snapshots yet");
    const h = await call("GET", `/v1/volumes/${encodeURIComponent(vol)}/history`);
    if (h.status >= 400) return { ...text(`${h.status}: ${typeof h.data === "string" ? h.data : JSON.stringify(h.data)}`), isError: true };
    const rows = (Array.isArray(h.data) ? h.data : []) as { id: string; message?: string; createdAt?: string; phase?: string }[];
    return text(rows.length ? rows.map((r) => ({ snapshot: r.id, message: r.message || undefined, taken: r.createdAt, state: r.phase })) : "no snapshots yet");
  };
  /**
   * A lifecycle verb: do it, then wait for it. The answer is the FINAL document, so the model has
   * no reason to poll and nothing to guess; a wait that runs out says what state it is still in
   * and hands back that document, which is a fact rather than a failure.
   */
  const after = async (kind: "workspaces" | "environments", started: { status: number; data: unknown }, id: string, cap: number, signal?: AbortSignal) => {
    if (started.status >= 400) return { ...text(`${started.status}: ${typeof started.data === "string" ? started.data : JSON.stringify(started.data)}`), isError: true };
    const r = await settle(() => call("GET", `/v1/${kind}/${encodeURIComponent(id)}`), (d) => RESTING.has(String(d?.state ?? "")), cap, signal);
    const state = String((r.data as any)?.state ?? "");
    // "still ready after 182s" is a contradiction, and it is what a wait that ran out printed when
    // the state it was waiting for was one it did not know (transcripts, 2026-09-18). A wait that
    // ends on a state we call settled IS settled; anything else says what it is still doing.
    if (r.settled || RESTING.has(state)) return text(r.data);
    return text(`still ${state || "working"} after ${Math.round(r.waitedMs / 1000)}s\n${JSON.stringify(r.data, null, 2)}`);
  };
  /**
   * The two things a person never types. Where their work runs is where THIS machine runs, and
   * whose it is is the space they are in — asking the model for a region id or an owner slug is
   * asking it to look them up first, which is exactly the "checked the quota before creating"
   * the owner objected to.
   */
  let ownRegion: string | undefined;
  const region = async (): Promise<string | undefined> => {
    const own = process.env.KL_WORKSPACE_ID;
    if (ownRegion || !own) return ownRegion;
    const r = await call("GET", `/v1/workspaces/${encodeURIComponent(own)}`);
    return (ownRegion = (r.data as { region?: string } | null)?.region);
  };
  /** A team space owns what is made in it; a personal one is the caller's own, which /v1 defaults to. */
  const owner = () => {
    const space = process.env.KL_TEAM;
    return space && space !== process.env.KL_OWNER ? space : undefined;
  };
  /**
   * An id, or the NAME a person calls it. Every listing already carries both, so a name costs one
   * GET and saves the model a lookup it would otherwise do out loud. Ambiguity is refused rather
   * than guessed — two workspaces called "api" is exactly when picking one is wrong.
   */
  const ws = async (a: Record<string, any>) => refuseOwnBench(await resolveNamed("workspaces", refuseOwnBench(String(a.id))));
  const env = (a: Record<string, any>) => resolveNamed("environments", String(a.id));

  /** The id a create answered with, or the one it was given. */
  const idOf = (started: { data: unknown }, fallback = "") => String((started.data as any)?.id ?? fallback);
  /**
   * A push answers `{id, phase}` and the cut happens after. The snapshot is only findable through
   * its volume, and `/v1` hands the volume name back in the listing, not in the parent's document —
   * so that is resolved once and then only the history is polled.
   */
  const afterPush = async (kind: "workspaces" | "environments", started: { status: number; data: unknown }, parent: string, signal?: AbortSignal) => {
    if (started.status >= 400) return { ...text(`${started.status}: ${typeof started.data === "string" ? started.data : JSON.stringify(started.data)}`), isError: true };
    const snap = idOf(started);
    const vols = await call("GET", "/v1/volumes");
    const vol = (vols.data as { name?: string; volume?: string }[] | null)?.find((v) => v.name === parent)?.volume;
    if (!vol || !snap) return text(started.data ?? "pushed");
    const ready = (rows: any) => Array.isArray(rows) && rows.some((r) => r.id === snap && String(r.phase).toLowerCase() === "ready");
    const r = await settle(() => call("GET", `/v1/volumes/${encodeURIComponent(vol)}/history`), ready, CAP.push, signal);
    const row = (r.data as any[] | undefined)?.find?.((x) => x.id === snap);
    return text(r.settled && ready(r.data) ? { snapshot: snap, volume: vol, phase: "Ready", message: row?.message } : `snapshot ${snap} still ${row?.phase ?? "being cut"} after ${Math.round(r.waitedMs / 1000)}s`);
  };
  const O = <T>(t: T) => Type.Optional(t as any);

  // workspaces
  reg("kl_workspaces", { team: O(S("team slug; absent = personal")) }, (a) => shared("workspace.list", a));
  reg("kl_workspace", { id: S("workspace id or name") }, (a) => shared("workspace.inspect", a));
  reg(
    "kl_workspace_create",
    {
      name: S("workspace name"),
      repo: O(S("repository to start from, e.g. kloudlite/rustic-git")),
      branch: O(S("branch to check out")),
      packages: O(Type.Array(Type.String(), { description: "nixpkgs ATTRIBUTE names, not language names: rustc cargo (Rust), nodejs_22, go, python3, bun, pnpm, jdk21, gcc, gnumake; `attr@version` pins one" })),
      from_snapshot: O(S("a snapshot id to start from instead of an empty workspace")),
    },
    async (a, signal) => {
      // One verb for a person: "make me a workspace", from nothing or from a snapshot they named.
      // A repo+branch and a snapshot in the same call is two sources for one workspace: the
      // snapshot silently won, and the repository the person named was never cloned. Refused
      // here, before the platform is called, in the same words the capability contract states.
      const sources = workspaceCreateSourceIssues(a as Record<string, JsonValue>);
      if (sources.length) return { ...text(sources.map((issue) => issue.message).join("; ")), isError: true };
      const result = await shared("workspace.create", a.from_snapshot ? a : { ...a, region: await region(), team: owner(), quota_gb: 20 });
      if (result.isError) return result;
      return after("workspaces", { status: 200, data: JSON.parse(result.content[0].text) }, idOf({ data: JSON.parse(result.content[0].text) }), CAP.create, signal);
    },
  );
  reg("kl_workspace_start", { id: WS }, async (a, signal) => { const id = await ws(a); return after("workspaces", await call("POST", `/v1/workspaces/${id}/start`), id, CAP.start, signal); });
  reg("kl_workspace_stop", { id: WS }, async (a) => answer("POST", `/v1/workspaces/${await ws(a)}/stop`));
  reg("kl_workspace_snapshot", { id: WS, message: O(S("what this snapshot is")) }, async (a, signal) => {
    const id = await ws(a);
    return afterPush("workspaces", await call("POST", `/v1/workspaces/${id}/push`, { message: a.message }), id, signal);
  });
  reg("kl_workspace_snapshots", { id: WS }, async (a) => snapshotsOf("workspaces", await ws(a)));
  reg("kl_workspace_clone", { id: WS, name: S("name for the clone") }, async (a) => answer("POST", `/v1/workspaces/${await ws(a)}/clone`, { name: a.name }));
  reg("kl_workspace_delete", { id: WS }, async (a) => answer("DELETE", `/v1/workspaces/${await ws(a)}`));

  // environments
  environmentTools(reg);
  reg(
    "kl_environment_create",
    { name: S("environment name"), services: O(Type.Array(SERVICE, { description: "services to run" })), from_snapshot: O(S("a snapshot id to start from instead of a services list")) },
    async (a, signal) => {
      const environmentOwner = owner();
      const environmentRegion = await region();
      const result = await shared("environment.create", { ...a, ...(environmentOwner ? { owner: environmentOwner } : {}), ...(environmentRegion ? { region: environmentRegion } : {}), ...(a.services ? { services: a.services.map(service) } : {}) });
      if (result.isError) return result;
      const data = JSON.parse(result.content[0].text);
      return after("environments", { status: 200, data }, idOf({ data }), CAP.create, signal);
    },
  );
  reg("kl_environment_start", { id: ENV }, async (a, signal) => { const id = await env(a); return after("environments", await call("POST", `/v1/environments/${id}/start`), id, CAP.start, signal); });
  reg("kl_environment_stop", { id: ENV }, async (a) => answer("POST", `/v1/environments/${await env(a)}/stop`));
  reg("kl_environment_snapshot", { id: ENV, message: O(S("what this snapshot is")) }, async (a, signal) => {
    const id = await env(a);
    return afterPush("environments", await call("POST", `/v1/environments/${id}/push`, { message: a.message }), id, signal);
  });
  reg("kl_environment_snapshots", { id: ENV }, async (a) => snapshotsOf("environments", await env(a)));
  reg("kl_environment_clone", { id: ENV, name: S("name for the clone") }, async (a) => answer("POST", `/v1/environments/${await env(a)}/clone`, { name: a.name }));
  // Restore means "put it back": into the environment the person named, not into a new one.
  reg("kl_environment_restore", { id: ENV, snapshot: S("snapshot id to go back to") }, async (a, signal) => {
    const id = await env(a);
    const result = await shared("environment.restore", { id, snapshot: a.snapshot });
    if (result.isError) return result;
    return after("environments", { status: 200, data: JSON.parse(result.content[0].text) }, id, CAP.restore, signal);
  });
  reg("kl_environment_delete", { id: ENV }, async (a) => answer("DELETE", `/v1/environments/${await env(a)}`));

  repoTools(reg);
  imageTools(reg);
  progressTool(reg);
  planTools(reg);
  if (process.env.KL_EPHEMERAL !== "1") agentTools(reg, process.env.KL_WORKSPACE_ID);
  searchTools(reg, pi);
  memoryTools(reg);
  architectureTools(reg);
  questionTool(reg, pi);
  packageTools(reg);
  spaceTools(reg, process.env.KL_TEAM);
  capabilities(reg);
  modeCommand(pi, PLAN_TOOLS);
  // A bench session's active set has no local hands in it (spec §3.1).
  startWith(pi, BENCH_ALWAYS_ON);
}

/**
 * The active set is applied on SESSION START, never at load. `registerTool` works while an
 * extension loads; an action method does not — `setActiveTools` at load threw "Extension runtime
 * not initialized" and every bench session on the fleet exited (2026-09-17). Registering is
 * describing; activating is doing, and doing waits for a session.
 */
function startWith(pi: ExtensionAPI, on: string[] = ALWAYS_ON) {
  const start = on.filter((n) => !n.startsWith("ask") || process.env.KL_EPHEMERAL !== "1");
  pi.on("session_start", async () => {
    // What this session has already found is part of what it starts with: a tool_search hit lasts
    // the session, and a bench restart is not the end of one.
    const found = await foundHere();
    const registered = new Set((pi.getAllTools?.() ?? []).map((t: { name: string } | string) => (typeof t === "string" ? t : t.name)));
    const armed = found.filter((n) => !registered.size || registered.has(n));
    pi.setActiveTools?.([...new Set([...start, ...armed])]);
  });
}

/** The session this child is, as the bench named it at spawn; absent for anything not a session. */
const ownSession = () => process.env.KL_SESSION;

/** What `tool_search` has already turned on for this session, from the bench. */
async function foundHere(): Promise<string[]> {
  const id = ownSession();
  if (!id) return [];
  const r = await benchCall("GET", `/sessions/${encodeURIComponent(id)}/found`).catch(() => ({ ok: false, data: {} }));
  return r.ok && Array.isArray(r.data?.found) ? (r.data.found as string[]) : [];
}

/** Record what a search just found, so the next session starts with it. Never fails a search. */
async function rememberFound(names: string[]): Promise<void> {
  const id = ownSession();
  if (!id || !names.length) return;
  await benchCall("POST", `/sessions/${encodeURIComponent(id)}/found`, { names }).catch(() => undefined);
}

/**
 * Two modes, one file. A WORKSPACE session already has hands on its own files
 * (`workspace-tools.ts`); all it gains here is the machine's own packages, so
 * "install ripgrep" inside a workspace is that workspace's, not a platform call
 * about somebody else. A BENCH session gets the platform, and asks.
 */
export default function (pi: ExtensionAPI) {
  // A `btw` fork runs `--no-tools` over a copy of a session's transcript. It registers nothing —
  // it is loaded ONLY so it is told what it is, like every other session (owner, 2026-09-17).
  // A fork has no tools at all, so the list of what the tools do would be a list of lies.
  if (process.env.KL_FORK === "1") return tellItWhereItStands(pi, "You are the Kloudlite harness. You answer one question about this bench's work, from the transcript you were forked from. You have no tools: you can change nothing, and you cannot look anything up — answer from what is in front of you, or say it is not there.", false);
  // The mode is which machine's session this is: a workspace names it, the bench is its own
  // (`KL_WORKSPACE_ID`, whose tool server `workspace-tools.ts` is pointed at by address).
  const inWorkspace = process.env.KL_TOOLS_WORKSPACE;
  if (inWorkspace) {
    const reg = makeReg(pi);
    ownTools(pi, inWorkspace, process.env.KL_TEAM, reg);
    environmentTools(reg);
    repoTools(reg);
    progressTool(reg);
    planTools(reg);
    // A workspace may LOOK at the platform — which workspaces exist, what an environment runs —
    // without being able to change anybody else's. Registered here, inactive until `tool_search`
    // finds one (owner, 2026-09-17).
    lookAround(reg);
    architectureTools(reg);
    // An ask is a conversation: its decision, then its result (§3.8).
    reportTool(reg);
    // An agent is a session with one task: it reports to whoever started it and starts nobody.
    if (process.env.KL_EPHEMERAL !== "1") agentTools(reg, inWorkspace);
    searchTools(reg, pi);
    memoryTools(reg);
    questionTool(reg, pi);
    capabilities(reg);
    modeCommand(pi, PLAN_TOOLS);
    return startWith(pi);
  }
  // A bench session has NO hands where it runs (spec §3.1): no ide tools on this container, no
  // package tools bound to "here", no shell. Everything it does to the world is a `kl_*` call or a
  // message to a session that does have hands.
  tools(pi);
  tellItWhereItStands(pi, BENCH_HANDS);
}
