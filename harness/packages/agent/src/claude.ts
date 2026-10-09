/**
 * Claude models, through Claude Code.
 *
 * Ruling: Claude in the harness runs ONLY through the Claude Agent SDK, signed
 * in by Claude Code's own login (`claude auth login`, run by the daemon
 * under a pty: backend `claudelogin.ts`). Never through pi-ai's anthropic OAuth, which
 * spoofs Claude Code headers. So `provider === "anthropic"` models are served
 * here, every other provider by pi (`index.ts`).
 *
 * Fast: ONE `claude` child per harness session, kept alive across messages.
 * The SDK's `query()` takes an async iterable of user messages; we never end it
 * until `dispose()`, so a new prompt (or a mid-turn steer) is just a push - no
 * respawn, no `--resume` per message. (The pi-claude-agent-sdk bridge ends its
 * input stream after every result, which costs a process start per message;
 * that is the behaviour this file exists to avoid.) If the child dies, the next
 * prompt starts a fresh query resumed from pi's record.
 *
 * Shape: this implements the subset of pi's `AgentSession` that
 * `apps/tui/src/app.tsx` touches (subscribe, prompt, steer, followUp,
 * clearQueue, abort, dispose, setModel, setThinkingLevel,
 * setAutoCompactionEnabled, messages, agent.beforeToolCall) and emits the same
 * `AgentSessionEvent` shapes, so the TUI renders it unchanged.
 *
 * Tools and transcript are pi's (spec 2026-10-08-claude-tool-host); the
 * system prompt is Claude Code's preset. The pi AgentSession is built as for
 * any model but its loop never runs: its declared tools are served to the
 * child over in-process MCP (`claude-tools.ts`), and every finished message is recorded into pi's file and `agent.state`.
 * Each query start resumes Claude Code from pi's record
 * (`claude-history.ts` through `sessionStore`), so switching to or from a
 * pi model never loses a turn. Claude Code's built-in tools are off.
 */
import { randomUUID } from "node:crypto";
import { execFile } from "node:child_process";
import { createRequire } from "node:module";
import { appendFile, mkdir } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { query as sdkQuery, type Options, type SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import { BTW_SYSTEM, btwPrompt } from "./btw.ts";
import { TOOL_PREFIX, toClaudeEntries } from "./claude-history.ts";
import { createToolServer, declaredTools } from "./claude-tools.ts";

// Claude sessions run Claude Code's loop, which never sees pi's skills; ours reach it as a local
// plugin: `kl:kloudlite` (platform context) in every session, plus the session role's own skill
// (`kl:main-session` / `kl:workspace-session`), `kl:codemode` only while codemode is a tool.
const KL_PLUGIN = fileURLToPath(new URL("../claude-plugin", import.meta.url));
export const CODEMODE_SKILL = join(KL_PLUGIN, "skills", "codemode");
export const KLOUDLITE_SKILL = join(KL_PLUGIN, "skills", "kloudlite");
export type Role = "main" | "workspace";
export const roleSkill = (role: Role) => join(KL_PLUGIN, "skills", `${role}-session`);

export const AUTH_MESSAGE = "Claude is not signed in on this bench — run /login in the TUI and pick Claude (subscription).";

type Level = "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";
type Effort = NonNullable<Options["effort"]>;

/** 1:1 onto the SDK's effort; `minimal` has no match and takes `low`; `off` is no effort (thinking disabled). */
export function effortFor(level: Level | undefined): Effort | undefined {
  if (!level || level === "off") return undefined;
  return level === "minimal" ? "low" : level;
}

/** process.env minus everything that would route Claude Code off its own login. */
export function claudeEnv(env: NodeJS.ProcessEnv = process.env): Record<string, string | undefined> {
  const out: Record<string, string | undefined> = {};
  for (const [k, v] of Object.entries(env)) {
    if (
      k.startsWith("ANTHROPIC_") ||
      k === "CLAUDE_CODE_OAUTH_TOKEN" ||
      k === "CLAUDE_CODE_USE_BEDROCK" ||
      k === "CLAUDE_CODE_USE_VERTEX" ||
      k === "CLAUDE_CODE_USE_FOUNDRY"
    )
      continue;
    out[k] = v;
  }
  out.ENABLE_CLAUDEAI_MCP_SERVERS = "0";
  out.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "1";
  return out;
}

// ---- login status -----------------------------------------------------------

/** The SDK's bundled `claude`, else whatever is on PATH. */
export function claudeBinary(): string {
  const require = createRequire(import.meta.url);
  const arch = process.arch;
  const names =
    process.platform === "linux"
      ? [`linux-${arch}`, `linux-${arch}-musl`]
      : [`${process.platform}-${arch}`];
  for (const n of names) {
    try {
      return require.resolve(`@anthropic-ai/claude-agent-sdk-${n}/claude`);
    } catch {}
  }
  return "claude";
}

let signedIn: Promise<boolean> | undefined;

/**
 * `claude auth status --json`, run once and cached; `refresh` after the login
 * screen closes. Async so polling it never stalls the TUI's render loop.
 */
export function claudeSignedIn(refresh = false): Promise<boolean> {
  if (signedIn && !refresh) return signedIn;
  return (signedIn = new Promise<boolean>((resolve) => {
    execFile(
      claudeBinary(),
      ["auth", "status", "--json"],
      { timeout: 15_000, env: claudeEnv() as NodeJS.ProcessEnv },
      (err, stdout) => {
        try {
          resolve(!err && JSON.parse(stdout || "{}").loggedIn === true);
        } catch {
          resolve(false);
        }
      },
    );
  }));
}

// ---- session ------------------------------------------------------------------

type Listener = (event: any) => void;
type Image = { type: "image"; data: string; mimeType: string };
type QueryFn = (p: { prompt: AsyncIterable<SDKUserMessage> | string; options?: Options }) => AsyncIterable<any> & {
  interrupt(): Promise<unknown>;
  setModel(model?: string): Promise<void>;
  applyFlagSettings(s: any): Promise<void>;
  setMaxThinkingTokens(n: number | null, display?: string | null): Promise<void>;
  close?(): void;
};

export type PiHost = {
  agent: { state: { messages: any[]; tools: any[] }; beforeToolCall?: any; afterToolCall?: any };
  sessionManager: { appendMessage(m: any): unknown; buildSessionContext(): { messages: any[] } };
  subscribe(l: (e: any) => void): () => void;
  dispose?(): void;
  // pi 1.0.4 private input pipeline, see expand()
  _tryExecuteExtensionCommand?(text: string): Promise<boolean>;
  _queueUserInput?(text: string, images: any, behavior: "steer" | "followUp", source: string): Promise<string>;
};

export type ClaudeOptions = {
  key: string;
  model: { id: string };
  thinkingLevel?: Level;
  /** Which role skill the session loads beside `kl:kloudlite`. */
  role?: Role;
  /** The pi session whose tools, prompt and record this one runs on. */
  pi: PiHost;
  cwd?: string;
  /** Injected in tests so they need no binary and no network. */
  query?: QueryFn;
  /** One line per turn; default under ~/.cache (ignored on the bench), none with an injected query. */
  timingLog?: string;
};

export const TIMING_LOG = join(homedir(), ".cache", "kl-harness", "claude-timing.log");

type Timing = { t0: number; model: string; effort?: string; thinking?: number; text?: number; tool?: number };

/** Best effort: a lost line costs nothing, a thrown one would end the turn. */
function logTiming(path: string, t: Timing, result: any) {
  const line = JSON.stringify({
    ts: new Date(t.t0).toISOString(),
    model: t.model,
    effort: t.effort ?? null,
    first_thinking_ms: t.thinking ?? null,
    first_text_ms: t.text ?? null,
    first_tool_ms: t.tool ?? null,
    result_ms: Date.now() - t.t0,
    api_ms: result?.duration_api_ms ?? null,
    ok: result?.subtype === "success" && !result?.is_error,
  });
  void mkdir(dirname(path), { recursive: true })
    .then(() => appendFile(path, line + "\n"))
    .catch(() => {});
}

/** An async iterable we push into and only close on dispose. */
class Pushable<T> implements AsyncIterable<T> {
  private items: T[] = [];
  private wake?: () => void;
  private closed = false;
  push(v: T) {
    this.items.push(v);
    this.wake?.();
  }
  close() {
    this.closed = true;
    this.wake?.();
  }
  async *[Symbol.asyncIterator]() {
    for (;;) {
      if (this.items.length) yield this.items.shift()!;
      else if (this.closed) return;
      else await new Promise<void>((r) => (this.wake = r));
    }
  }
}

let lastStamp = 0;
/** Message ids in the TUI are the creation timestamp, so they must be unique. */
const stamp = () => (lastStamp = Math.max(Date.now(), lastStamp + 1));

export function createClaudeSession(opts: ClaudeOptions) {
  const run: QueryFn = opts.query ?? (sdkQuery as unknown as QueryFn);
  const listeners = new Set<Listener>();
  // a disposed session is retired: its file already belongs to the replacement, so a late tool result must not write
  let disposed = false;
  const emit = (e: any) => {
    if (disposed) return;
    for (const l of [...listeners]) l(e);
  };

  // the TUI is fullscreen, so timing goes to a file, never stderr
  const timingLog = opts.timingLog ?? (opts.query ? undefined : TIMING_LOG);
  let timing: Timing | undefined;
  const since = () => (timing ? Date.now() - timing.t0 : undefined);

  let model = opts.model.id;
  let effort = effortFor(opts.thinkingLevel);
  let thinkingOff = opts.thinkingLevel === "off";
  let autoCompact: boolean | undefined;
  let compacting = false;
  let retry: number | undefined;
  let compactReason: "manual" | "threshold" = "threshold";
  // the latest assistant message's tool calls and which have a result: a steer waits for the whole batch
  const batch = new Set<string>();
  const resulted = new Set<string>();

  const piSession = opts.pi;
  const record = (m: any) => {
    if (disposed) return;
    piSession.sessionManager.appendMessage(m);
    piSession.agent.state.messages = [...piSession.agent.state.messages, m];
  };
  let turn = new AbortController();
  // tool_use id -> the recorded assistant message that issued it (this turn)
  const issued = new Map<string, any>();
  const waiting = new Map<string, (m: any) => void>();
  const lastAssistant = () => piSession.agent.state.messages.findLast((m: any) => m.role === "assistant");
  /**
   * The MCP call can overtake our read of the stream, and the gate and
   * codemode's nested calls need the issuing message recorded first.
   * Resolves early with the last assistant message when the turn aborts.
   * ponytail: 5 s cap then the last assistant message; a stream that slow
   * has bigger problems.
   */
  const assistantFor = (id: string) =>
    issued.has(id)
      ? Promise.resolve(issued.get(id))
      : new Promise<any>((resolve) => {
          const sig = turn.signal;
          const done = (m: any) => (clearTimeout(t), sig.removeEventListener("abort", onAbort), waiting.delete(id), resolve(m));
          const onAbort = () => done(lastAssistant());
          const t = setTimeout(onAbort, 5_000);
          sig.addEventListener("abort", onAbort, { once: true });
          waiting.set(id, done);
          if (sig.aborted) onAbort();
        });
  // one server per query: an MCP Server binds one transport, and each start() spawns a new child
  const toolServer = () =>
    createToolServer({
      tools: () => declaredTools(piSession),
      agent: piSession.agent,
      assistantFor,
      signal: () => turn.signal,
      emit: (e) => {
        if (gaveUp.has(e.toolCallId)) return; // already closed by settleTools
        if (e.type === "tool_execution_start") {
          if (timing) timing.tool ??= since();
          let done!: () => void;
          inflight.set(e.toolCallId, { name: e.toolName, done: new Promise<void>((r) => (done = r)), resolve: done });
        }
        emit(e);
      },
      onResult: (m) => {
        const f = inflight.get(m.toolCallId);
        if (!f) return; // given up on: that call already has its synthetic result
        record(m);
        inflight.delete(m.toolCallId);
        f.resolve();
        resulted.add(m.toolCallId);
        flushSteers();
      },
    });
  // tool calls started and not yet resulted; a turn does not end before they do (pi's Promise.all)
  const inflight = new Map<string, { name: string; done: Promise<void>; resolve: () => void }>();
  const gaveUp = new Set<string>();
  /** Wait for running tools (the turn's signal is already aborted on abort/death); 5 s cap, then a synthetic error result each. */
  async function settleTools() {
    let t: any;
    await Promise.race([Promise.all([...inflight.values()].map((f) => f.done)), new Promise((r) => (t = setTimeout(r, 5_000)))]);
    clearTimeout(t);
    for (const [id, f] of inflight) {
      const content = [{ type: "text", text: "Operation aborted" }];
      gaveUp.add(id);
      emit({ type: "tool_execution_end", toolCallId: id, toolName: f.name, result: { content }, isError: true });
      record({ role: "toolResult", toolCallId: id, toolName: f.name, content, isError: true, timestamp: Date.now() });
    }
    inflight.clear();
  }
  // codemode's nested calls are emitted on the pi session itself
  const unsubPi = piSession.subscribe((e: any) => {
    if (e?.parentToolCallId && String(e.type).startsWith("tool_execution_")) emit(e);
  });

  let input: Pushable<SDKUserMessage> | undefined;
  let q: ReturnType<QueryFn> | undefined;
  let running = false;
  let aborting = false;
  let sawState = false;
  let rateLimited: any;
  const followUps: { text: string; images?: Image[] }[] = [];
  const steering: { text: string; images?: Image[] }[] = [];

  // streaming state for the assistant message in flight
  type Cur = { msg: any; parts: Map<number, any>; id?: string };
  let cur: Cur | undefined;
  const streamed = new Set<string>();

  const queueUpdate = () =>
    emit({ type: "queue_update", steering: steering.map((s) => s.text), followUp: followUps.map((f) => f.text) });

  const newMessage = (extra: object = {}) => ({
    role: "assistant",
    content: [] as any[],
    api: "anthropic-messages",
    provider: "anthropic",
    model,
    usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
    stopReason: "stop",
    timestamp: stamp(),
    ...extra,
  });

  // closing messages wait behind running tools so the record reads assistant, results, error
  const late: [string, string][] = [];
  function errorMessage(text: string, stopReason = "error") {
    if (inflight.size) return void late.push([text, stopReason]);
    const msg = newMessage({ stopReason, errorMessage: text });
    emit({ type: "message_start", message: msg });
    emit({ type: "message_end", message: msg });
    record(msg);
  }

  function endCur() {
    if (!cur) return;
    const msg = cur.msg;
    cur = undefined;
    // an abort between content_block_start and _stop leaves the partial-json accumulator behind
    for (const b of msg.content) if (b.type === "toolCall") delete b.json;
    record(msg);
    emit({ type: "message_end", message: msg });
    batch.clear();
    resulted.clear();
    for (const b of msg.content)
      if (b.type === "toolCall") {
        batch.add(b.id);
        issued.set(b.id, msg);
        waiting.get(b.id)?.(msg);
        waiting.delete(b.id);
      }
  }

  let finishing = false;
  function finish() {
    if (!running || finishing) return;
    if (!inflight.size) return conclude();
    finishing = true;
    void settleTools().then(conclude);
  }

  function conclude() {
    finishing = false;
    if (!running) return;
    // an aborted turn is ONE assistant message, as pi records it; none in flight gets an empty one
    const aborted = aborting;
    if (aborting && cur) cur.msg.stopReason = "aborted";
    const hadPartial = !!cur;
    endCur();
    for (const [t, r] of late.splice(0)) errorMessage(t, r);
    if (aborting && !hadPartial) errorMessage("", "aborted");
    aborting = false;
    issued.clear();
    batch.clear();
    resulted.clear();
    // never carry an open retry or compaction into the next turn
    if (retry !== undefined) {
      emit({ type: "auto_retry_end", success: false, attempt: retry, finalError: "Retry cancelled" });
      retry = undefined;
    }
    if (compacting) {
      compacting = false;
      emit({ type: "compaction_end", reason: compactReason, result: undefined, aborted: true, willRetry: false });
    }
    // pi keeps one run: a steer held at the end continues it, with no agent_end/agent_start between;
    // an abort stops the run and, as in pi (agent-session.js:1905), leaves both queues for the next prompt
    if (steering.length && input && !aborted) {
      const held = steering.splice(0);
      queueUpdate();
      timing = { t0: Date.now(), model, effort };
      for (const s of held) pushUser(s.text, s.images, true);
      return;
    }
    running = false;
    emit({ type: "agent_end", messages: [] });
    if (aborted) {
      if (steering.length || followUps.length) queueUpdate(); // still pending: tell the TUI
      return;
    }
    const dead = steering.splice(0);
    if (dead.length) {
      queueUpdate();
      for (const s of dead) void send(s.text, s.images, true);
      return;
    }
    const next = followUps.shift();
    if (next) {
      queueUpdate();
      void send(next.text, next.images);
    }
  }

  // pi's formula (pi-ai anthropic-messages): every token the request carried
  const usage = (u: any) => {
    const x = cur!.msg.usage;
    if (u.input_tokens != null) x.input = u.input_tokens;
    if (u.output_tokens != null) x.output = u.output_tokens;
    if (u.cache_read_input_tokens != null) x.cacheRead = u.cache_read_input_tokens;
    if (u.cache_creation_input_tokens != null) x.cacheWrite = u.cache_creation_input_tokens;
    x.totalTokens = x.input + x.output + x.cacheRead + x.cacheWrite;
  };

  function onStream(ev: any) {
    switch (ev?.type) {
      case "message_start": {
        cur = { msg: newMessage(), parts: new Map(), id: ev.message?.id };
        if (cur.id) streamed.add(cur.id);
        if (ev.message?.usage) usage(ev.message.usage);
        if (retry !== undefined) {
          emit({ type: "auto_retry_end", success: true, attempt: retry });
          retry = undefined;
        }
        emit({ type: "message_start", message: cur.msg });
        break;
      }
      case "content_block_start": {
        if (!cur) break;
        const b = ev.content_block;
        if (b?.type === "text") {
          const part = { type: "text", text: "" };
          cur.msg.content.push(part);
          cur.parts.set(ev.index, part);
        } else if (b?.type === "thinking") {
          const part = { type: "thinking", thinking: "", thinkingSignature: "" };
          cur.msg.content.push(part);
          cur.parts.set(ev.index, part);
        } else if (b?.type === "tool_use") {
          const part = { type: "toolCall", id: b.id, name: String(b.name).replace(TOOL_PREFIX, ""), arguments: {} as any, json: "" };
          cur.msg.content.push(part);
          cur.parts.set(ev.index, part);
        }
        break;
      }
      case "content_block_delta": {
        const part = cur?.parts.get(ev.index);
        if (!cur || !part) break;
        const d = ev.delta;
        if (d?.type === "text_delta") {
          part.text += d.text;
          if (timing) timing.text ??= since();
        } else if (d?.type === "thinking_delta") {
          part.thinking += d.thinking;
          if (timing) timing.thinking ??= since();
        }
        else if (d?.type === "signature_delta") {
          part.thinkingSignature += d.signature;
          break;
        } else if (d?.type === "input_json_delta") {
          part.json += d.partial_json;
          break;
        } else break;
        emit({ type: "message_update", message: cur.msg });
        break;
      }
      case "content_block_stop": {
        const part = cur?.parts.get(ev.index);
        if (part?.type === "toolCall") {
          try {
            part.arguments = part.json ? JSON.parse(part.json) : {};
          } catch {}
          delete part.json;
        }
        break;
      }
      case "message_delta": {
        if (!cur) break;
        const sr = ev.delta?.stop_reason;
        if (sr) {
          cur.msg.stopReason = sr === "tool_use" ? "toolUse" : sr === "max_tokens" ? "length" : sr === "refusal" ? "error" : "stop";
          if (sr === "refusal") cur.msg.errorMessage = "Refused";
        }
        if (ev.usage) usage(ev.usage);
        break;
      }
      case "message_stop":
        endCur();
        break;
    }
  }

  /** Non-streamed assistant message: only when no stream events preceded it. */
  function onAssistant(m: any) {
    if (m.error === "authentication_failed" || m.error === "oauth_org_not_allowed") {
      errorMessage(AUTH_MESSAGE);
      return;
    }
    if (m.message?.id && streamed.has(m.message.id)) return;
    const msg = newMessage();
    emit({ type: "message_start", message: msg });
    for (const b of m.message?.content ?? []) {
      if (b.type === "text") msg.content.push({ type: "text", text: b.text });
      else if (b.type === "thinking") msg.content.push({ type: "thinking", thinking: b.thinking, thinkingSignature: b.signature ?? "" });
      else if (b.type === "tool_use") msg.content.push({ type: "toolCall", id: b.id, name: String(b.name).replace(TOOL_PREFIX, ""), arguments: b.input ?? {} });
    }
    emit({ type: "message_update", message: msg });
    cur = { msg, parts: new Map() };
    endCur();
  }

  function onResult(m: any) {
    if (m.subtype !== "success" || m.is_error) {
      // an interrupt ends the turn with an error result; finish() records the partial message as aborted
      if (!aborting) {
        endCur();
        const auth = /log ?in|authenticat|credential|401/i.test(String(m.result ?? m.errors ?? ""));
        let text = auth ? AUTH_MESSAGE : (m.errors?.join?.("; ") || m.result || m.subtype || "request failed");
        if (rateLimited) {
          const at = rateLimited.resetsAt ? new Date(rateLimited.resetsAt * 1000).toLocaleTimeString() : "later";
          text = `Claude rate limited (${rateLimited.rateLimitType ?? "unknown"}), resets at ${at}`;
        }
        errorMessage(text);
        if (retry !== undefined) {
          emit({ type: "auto_retry_end", success: false, attempt: retry, finalError: text });
          retry = undefined;
        }
      }
    }
    rateLimited = undefined;
    if (timing && timingLog) logTiming(timingLog, timing, m);
    timing = undefined;
    // idle is the authoritative end of turn where the CLI reports it; a result
    // may also cover several merged steers, so only fall back to it otherwise
    if (!sawState) finish();
  }

  function handle(m: any) {
    switch (m?.type) {
      case "system":
        if (m.subtype === "session_state_changed") {
          sawState = true;
          if (m.state === "idle") finish();
          // ponytail: when to compact, what the summary says and the retry policy are Claude Code's; we only report them.
        } else if (m.subtype === "status" && m.status === "compacting") {
          compacting = true;
          compactReason = "threshold"; // we never trigger one ourselves
          emit({ type: "compaction_start", reason: compactReason });
        } else if (m.subtype === "status" && m.compact_result === "failed") {
          compacting = false;
          emit({
            type: "compaction_end",
            reason: compactReason,
            result: undefined,
            aborted: false,
            willRetry: false,
            errorMessage: `Auto-compaction failed: ${m.compact_error ?? "unknown error"}`,
          });
        } else if (m.subtype === "compact_boundary") {
          const meta = m.compact_metadata ?? {};
          if (!compacting) compactReason = meta.trigger === "manual" ? "manual" : "threshold";
          compacting = false;
          emit({
            type: "compaction_end",
            reason: compactReason,
            result: { tokensBefore: meta.pre_tokens, estimatedTokensAfter: meta.post_tokens },
            aborted: false,
            willRetry: false,
          });
        } else if (m.subtype === "api_retry") {
          retry = m.attempt;
          emit({ type: "auto_retry_start", attempt: m.attempt, maxAttempts: m.max_retries, delayMs: m.retry_delay_ms, errorMessage: m.error_message ?? m.message ?? ([m.error_status, m.error].filter((x) => x != null).join(" ") || "Unknown error") });
        }
        break;
      case "stream_event":
        if (!m.parent_tool_use_id) onStream(m.event);
        break;
      case "assistant":
        if (!m.parent_tool_use_id) onAssistant(m);
        break;
      case "rate_limit_event":
        if (m.rate_limit_info?.status === "rejected") rateLimited = m.rate_limit_info;
        break;
      case "result":
        onResult(m);
        break;
    }
  }

  function start() {
    const stream = new Pushable<SDKUserMessage>();
    const cwd = opts.cwd ?? process.cwd();
    // a fresh id per query; Claude Code reads pi's record for it once, before the child spawns
    const resumeId = randomUUID();
    const history = toClaudeEntries(piSession.sessionManager.buildSessionContext().messages, { sessionId: resumeId, cwd, model });
    const codemode = declaredTools(piSession).some((t) => t.name === "codemode");
    const options: Options = {
      includePartialMessages: true,
      permissionMode: "bypassPermissions",
      allowDangerouslySkipPermissions: true,
      // `tools: []` removes the built-in Skill tool and `skills` alone does not bring it back
      // (probed against the SDK 2026-10-08: "Skill is disabled for this session"); naming it does.
      // Always on: every session needs the Kloudlite skill (the prompt below stays the untouched
      // preset for billing, so a skill is the only channel for platform context)
      tools: ["Skill"],
      // ponytail: one day per call so a long `bash` is never cut off by
      // Claude Code's MCP timeout; our abort is the real bound
      mcpServers: { kl: { type: "sdk", name: "kl", instance: toolServer() as any, timeout: 86_400_000 } as any },
      // Claude Code's own prompt, untouched: with pi's prompt in its place (or
      // appended) the server bills the session as a third-party app
      systemPrompt: { type: "preset", preset: "claude_code" },
      // the workspace's CLAUDE.md and .claude/, loaded by Claude Code itself
      settingSources: ["project"],
      plugins: [{ type: "local" as const, path: KL_PLUGIN }],
      skills: ["kl:kloudlite", ...(opts.role ? [`kl:${opts.role}-session`] : []), ...(codemode ? ["kl:codemode"] : [])],
      cwd,
      model,
      extraArgs: { "thinking-display": "summarized" },
      env: claudeEnv(),
      sessionStore: {
        append: async () => {},
        load: async (k: any) => (k.sessionId === resumeId && !k.subpath ? history : null),
      } as any,
      ...(thinkingOff ? { thinking: { type: "disabled" as const } } : {}),
      ...(effort ? { effort } : {}),
      ...(history.length ? { resume: resumeId } : {}),
    };
    input = stream;
    const mine = (q = run({ prompt: stream, options }));
    if (autoCompact !== undefined) void q.applyFlagSettings({ autoCompactEnabled: autoCompact }).catch(() => {});
    sawState = false;
    void (async () => {
      try {
        for await (const m of mine) {
          if (q !== mine) return;
          handle(m);
        }
      } catch (err) {
        if (!disposed && q === mine) {
          const text = String((err as Error)?.message ?? err);
          errorMessage(/log ?in|authenticat|credential|401/i.test(text) ? AUTH_MESSAGE : text);
        }
      }
      if (q === mine) {
        // the child is gone; the next prompt starts a new query resumed from pi's record
        q = undefined;
        input = undefined;
        turn.abort();
        if (!disposed) finish();
      }
    })();
  }

  /** Held steers go in at a boundary we choose, so clearQueue can still recall them. */
  function flushSteers() {
    if (!steering.length || !input || inflight.size || ![...batch].every((id) => resulted.has(id))) return;
    for (const s of steering.splice(0)) pushUser(s.text, s.images, true);
    queueUpdate();
  }

  /** `announce` is kept for the call sites; every user message is announced now, as pi's loop does
   * for a prompt and a followUp: workspace_ask arms its answer on the asked message's `message_end`
   * (delegate.ts), so a prompt or a queued followUp must emit one too. The TUI ignores user ones. */
  function pushUser(text: string, images?: Image[], _announce = false) {
    const content: any = images?.length
      ? [
          ...images.map((i) => ({ type: "image", source: { type: "base64", media_type: i.mimeType, data: i.data } })),
          { type: "text", text },
        ]
      : text;
    const msg = { role: "user", content: [{ type: "text", text }, ...(images ?? [])], timestamp: stamp() };
    emit({ type: "message_start", message: msg });
    record(msg);
    emit({ type: "message_end", message: msg });
    input!.push({ type: "user", message: { role: "user", content }, parent_tool_use_id: null });
  }

  async function send(text: string, images?: Image[], announce = false) {
    if (!q) start();
    const fresh = !running;
    if (fresh) {
      gaveUp.clear(); // late results of the last run are over by now; a new run's ids are new
      turn = new AbortController();
      timing = { t0: Date.now(), model, effort };
      running = true;
      emit({ type: "agent_start" });
    }
    pushUser(text, images, announce);
    // pi's run start drains the steering queue after the prompt (agent-loop.js:42-56, 68-69)
    if (fresh && steering.length) {
      for (const s of steering.splice(0)) pushUser(s.text, s.images, true);
      queueUpdate();
    }
  }

  /**
   * pi's own input steps (agent-session.js:1513-1545, 1686-1702): extension
   * commands, input handlers, /skill: and prompt templates. `undefined` = handled, send nothing.
   * ponytail: reads pi 1.0.4 private methods (expandPromptTemplate is not exported); a pin bump re-checks them.
   * _queueUserInput ends in _queueSteer/_queueFollowUp, which a shadow object captures instead of queueing into pi's idle agent.
   */
  async function expand(text: string, images: Image[] | undefined, behavior: "steer" | "followUp", command: boolean) {
    const p = piSession;
    if (!p._queueUserInput) return { text, images };
    if (command && text.startsWith("/") && (await p._tryExecuteExtensionCommand?.(text))) return undefined;
    let out: { text: string; images?: Image[] } | undefined;
    const capture = { value: async (t: string, i?: Image[]) => void (out = { text: t, images: i }) };
    const shadow = Object.create(p, { _queueSteer: capture, _queueFollowUp: capture });
    await p._queueUserInput.call(shadow, text, images, behavior, "interactive");
    return out;
  }

  return {
    isClaude: true as const,
    agent: piSession.agent,
    /** The pi session under it, for the permission gate on codemode's nested calls (local.ts). */
    pi: piSession,
    get messages(): any[] {
      return piSession.agent.state.messages;
    },
    get model() {
      return { provider: "anthropic", id: model };
    },
    subscribe(l: Listener) {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    async prompt(text: string, o?: { images?: Image[] }) {
      const x = await expand(text, o?.images, "steer", true);
      if (x) await send(x.text, x.images);
    },
    /** Held until a tool result or the turn end (pi's boundaries); clearQueue drops it unrecorded. */
    async steer(text: string, images?: Image[]) {
      const x = await expand(text, images, "steer", false);
      if (!x) return;
      // idle with steers left by an abort: queue behind them, as pi does
      if (!running && !steering.length) return send(x.text, x.images);
      steering.push(x);
      queueUpdate();
    },
    /** Side question: a separate one-shot query. Never touches `q`, `input`, `emit`, `record`, the
     * listeners or pi's record, so nothing it says is saved. */
    async btw(question: string): Promise<string> {
      const prompt = `${BTW_SYSTEM}\n\n${btwPrompt(piSession.agent.state.messages, question, 200_000)}`;
      const options: Options = {
        systemPrompt: { type: "preset", preset: "claude_code" }, // billing: same reason as start()
        tools: [],
        settingSources: [],
        maxTurns: 1,
        persistSession: false,
        cwd: opts.cwd ?? process.cwd(),
        model,
        env: claudeEnv(),
        ...(thinkingOff ? { thinking: { type: "disabled" as const } } : {}),
      };
      const auth = (t: string) => (/log ?in|authenticat|credential|401/i.test(t) ? AUTH_MESSAGE : t);
      try {
        for await (const m of run({ prompt, options })) {
          if (m.type !== "result") continue;
          if (m.subtype === "success" && !m.is_error) return String(m.result ?? "").trim() || "(no answer)";
          throw new Error(String(m.errors?.join?.("; ") || m.result || m.subtype || "request failed"));
        }
      } catch (err) {
        throw new Error(auth(String((err as Error)?.message ?? err)));
      }
      return "(no answer)";
    },
    /** Held here until the turn ends, then sent as its own prompt. */
    async followUp(text: string, images?: Image[]) {
      const x = await expand(text, images, "followUp", false);
      if (!x) return;
      if (!running) return send(x.text, x.images);
      followUps.push(x);
      queueUpdate();
    },
    clearQueue() {
      steering.length = 0;
      followUps.length = 0;
      queueUpdate();
    },
    async abort() {
      if (!running || !q) return;
      aborting = true;
      turn.abort();
      await q.interrupt().catch(() => {});
    },
    async setModel(m: { id: string }) {
      model = m.id;
      await q?.setModel(m.id).catch(() => {});
    },
    /**
     * Live via applyFlagSettings (effortLevel accepts 'max'); the replacement
     * of a dead child starts with the current effort and thinking in its options.
     */
    setThinkingLevel(level: Level) {
      thinkingOff = level === "off";
      effort = effortFor(level);
      // live: 0 disables thinking, null re-enables it (display as at start)
      void q?.setMaxThinkingTokens(thinkingOff ? 0 : null, thinkingOff ? undefined : "summarized").catch(() => {});
      void q?.applyFlagSettings({ effortLevel: effort ?? null }).catch(() => {});
    },
    /** Live: Claude Code compacts, we only switch it. */
    setAutoCompactionEnabled(on: boolean) {
      autoCompact = on;
      void q?.applyFlagSettings({ autoCompactEnabled: on }).catch(() => {});
    },
    dispose() {
      disposed = true; // first: nothing a running tool does from here on may touch the transcript
      unsubPi();
      turn.abort();
      input?.close();
      q?.close?.();
      listeners.clear();
      piSession.dispose?.();
    },
  };
}

export type ClaudeSession = ReturnType<typeof createClaudeSession>;
