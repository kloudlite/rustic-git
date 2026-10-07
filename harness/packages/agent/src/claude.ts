/**
 * Claude models, through Claude Code.
 *
 * Ruling: Claude in the harness runs ONLY through the Claude Agent SDK, signed
 * in by Claude Code's own login (`claude auth login`, run over ssh by
 * `kl-connect claude login`). Never through pi-ai's anthropic OAuth, which
 * spoofs Claude Code headers. So `provider === "anthropic"` models are served
 * here, every other provider by pi (`index.ts`).
 *
 * Fast: ONE `claude` child per harness session, kept alive across messages.
 * The SDK's `query()` takes an async iterable of user messages; we never end it
 * until `dispose()`, so a new prompt (or a mid-turn steer) is just a push - no
 * respawn, no `--resume` per message. (The pi-claude-agent-sdk bridge ends its
 * input stream after every result, which costs a process start per message;
 * that is the behaviour this file exists to avoid.) If the child dies, the next
 * prompt starts a fresh query with `resume: <claude session id>`.
 *
 * Shape: this implements the subset of pi's `AgentSession` that
 * `apps/tui/src/app.tsx` touches (subscribe, prompt, steer, followUp,
 * clearQueue, abort, dispose, setModel, setThinkingLevel,
 * setAutoCompactionEnabled, messages, agent.beforeToolCall) and emits the same
 * `AgentSessionEvent` shapes, so the TUI renders it unchanged.
 *
 * Tools are Claude Code's built-ins, run by the child itself with
 * `bypassPermissions` (the bench is the sandbox). Our tool `registry` is not
 * bridged: pi tools and Claude Code tools are different worlds, and an MCP
 * bridge would put a second hop in every call. The TUI sees them as
 * `tool_execution_*` events built from the stream's tool_use / tool_result.
 *
 * History: Claude Code holds the transcript (`resume`); `messages` stays empty,
 * so a reopened session shows nothing extra but remembers everything.
 */
import { execFile } from "node:child_process";
import { createRequire } from "node:module";
import { query as sdkQuery, type Options, type SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";

export const AUTH_MESSAGE = "Not signed in to Claude. On your laptop run: kl-connect claude login";

type Level = "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";
type Effort = NonNullable<Options["effort"]>;

/** off/minimal/low -> low, medium -> medium, high -> high, xhigh/max -> max. */
export function effortFor(level: Level | undefined): Effort | undefined {
  if (!level) return undefined;
  if (level === "off" || level === "minimal" || level === "low") return "low";
  if (level === "xhigh" || level === "max") return "max";
  return level;
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
type QueryFn = (p: { prompt: AsyncIterable<SDKUserMessage>; options?: Options }) => AsyncIterable<any> & {
  interrupt(): Promise<unknown>;
  setModel(model?: string): Promise<void>;
  applyFlagSettings(s: any): Promise<void>;
  close?(): void;
};

export type ClaudeOptions = {
  key: string;
  model: { id: string };
  fresh?: boolean;
  thinkingLevel?: Level;
  /** Where the Claude session id is kept (the key's meta.json). */
  store: { get(): string | undefined; set(id: string | undefined): void };
  cwd?: string;
  /** Injected in tests so they need no binary and no network. */
  query?: QueryFn;
};

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

const pi = (name: string) => name.toLowerCase();

/** Claude Code tool args in the shape the TUI's summaries and diffs read. */
function piArgs(name: string, a: any): any {
  if (!a || typeof a !== "object") return a;
  const path = a.file_path ?? a.path;
  if (name === "Edit") return { path, edits: [{ oldText: a.old_string, newText: a.new_string }] };
  if (name === "Write") return { path, content: a.content };
  return path ? { ...a, path } : a;
}

function textOf(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content))
    return content
      .filter((b: any) => b?.type === "text")
      .map((b: any) => b.text)
      .join("\n");
  return "";
}

export function createClaudeSession(opts: ClaudeOptions) {
  const run: QueryFn = opts.query ?? (sdkQuery as unknown as QueryFn);
  const listeners = new Set<Listener>();
  const emit = (e: any) => {
    for (const l of [...listeners]) l(e);
  };

  let model = opts.model.id;
  let effort = effortFor(opts.thinkingLevel);
  let sessionId = opts.fresh ? undefined : opts.store.get();
  if (opts.fresh) opts.store.set(undefined);

  let input: Pushable<SDKUserMessage> | undefined;
  let q: ReturnType<QueryFn> | undefined;
  let disposed = false;
  let running = false;
  let aborting = false;
  let sawState = false;
  let rateLimited: any;
  const followUps: { text: string; images?: Image[] }[] = [];
  const steering: string[] = [];

  // streaming state for the assistant message in flight
  type Cur = { msg: any; parts: Map<number, any>; id?: string; input: number };
  let cur: Cur | undefined;
  const streamed = new Set<string>();
  const tools = new Map<string, string>(); // tool_use id -> name

  const queueUpdate = () =>
    emit({ type: "queue_update", steering: [...steering], followUp: followUps.map((f) => f.text) });

  const newMessage = (extra: object = {}) => ({
    role: "assistant",
    content: [] as any[],
    timestamp: stamp(),
    stopReason: "stop",
    usage: { totalTokens: 0 },
    ...extra,
  });

  function errorMessage(text: string, stopReason = "error") {
    const msg = newMessage({ stopReason, errorMessage: text });
    emit({ type: "message_start", message: msg });
    emit({ type: "message_end", message: msg });
  }

  function endCur() {
    if (!cur) return;
    emit({ type: "message_end", message: cur.msg });
    cur = undefined;
  }

  function finish() {
    if (!running) return;
    endCur();
    if (aborting) errorMessage("", "aborted");
    aborting = false;
    running = false;
    emit({ type: "agent_end", messages: [] });
    const next = followUps.shift();
    if (next) {
      queueUpdate();
      void send(next.text, next.images);
    }
  }

  function toolStart(id: string, name: string, args: any) {
    tools.set(id, name);
    emit({ type: "tool_execution_start", toolCallId: id, toolName: pi(name), args: piArgs(name, args) });
  }

  function onStream(ev: any) {
    switch (ev?.type) {
      case "message_start": {
        cur = { msg: newMessage({ model }), parts: new Map(), id: ev.message?.id, input: ev.message?.usage?.input_tokens ?? 0 };
        if (cur.id) streamed.add(cur.id);
        cur.msg.usage.totalTokens = cur.input + (ev.message?.usage?.output_tokens ?? 0);
        // the turn has moved on, so steers pushed earlier were taken in
        if (steering.length) {
          steering.length = 0;
          queueUpdate();
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
          const part = { type: "thinking", thinking: "" };
          cur.msg.content.push(part);
          cur.parts.set(ev.index, part);
        } else if (b?.type === "tool_use") {
          cur.parts.set(ev.index, { tool: true, id: b.id, name: b.name, json: "" });
        }
        break;
      }
      case "content_block_delta": {
        const part = cur?.parts.get(ev.index);
        if (!cur || !part) break;
        const d = ev.delta;
        if (d?.type === "text_delta") part.text += d.text;
        else if (d?.type === "thinking_delta") part.thinking += d.thinking;
        else if (d?.type === "input_json_delta") {
          part.json += d.partial_json;
          break;
        } else break;
        emit({ type: "message_update", message: cur.msg });
        break;
      }
      case "content_block_stop": {
        const part = cur?.parts.get(ev.index);
        if (part?.tool) {
          let args: any = {};
          try {
            args = part.json ? JSON.parse(part.json) : {};
          } catch {}
          toolStart(part.id, part.name, args);
        }
        break;
      }
      case "message_delta": {
        if (!cur) break;
        const u = ev.usage;
        if (u) {
          cur.input = u.input_tokens ?? cur.input;
          cur.msg.usage.totalTokens = cur.input + (u.output_tokens ?? 0) + (u.cache_read_input_tokens ?? 0);
        }
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
    const msg = newMessage({ model });
    emit({ type: "message_start", message: msg });
    for (const b of m.message?.content ?? []) {
      if (b.type === "text") msg.content.push({ type: "text", text: b.text });
      else if (b.type === "thinking") msg.content.push({ type: "thinking", thinking: b.thinking });
      else if (b.type === "tool_use") toolStart(b.id, b.name, b.input);
    }
    emit({ type: "message_update", message: msg });
    emit({ type: "message_end", message: msg });
  }

  function onUser(m: any) {
    const content = m.message?.content;
    if (!Array.isArray(content)) return;
    for (const b of content) {
      if (b?.type !== "tool_result" || !tools.has(b.tool_use_id)) continue;
      const name = tools.get(b.tool_use_id)!;
      tools.delete(b.tool_use_id);
      emit({
        type: "tool_execution_end",
        toolCallId: b.tool_use_id,
        toolName: pi(name),
        isError: !!b.is_error,
        result: { content: [{ type: "text", text: textOf(b.content) }] },
      });
    }
  }

  function onResult(m: any) {
    if (m.subtype !== "success" || m.is_error) {
      endCur();
      // an interrupt ends the turn with an error result; finish() reports it as aborted
      if (!aborting) {
        const auth = /log ?in|authenticat|credential|401/i.test(String(m.result ?? m.errors ?? ""));
        let text = auth ? AUTH_MESSAGE : (m.errors?.join?.("; ") || m.result || m.subtype || "request failed");
        if (rateLimited) {
          const at = rateLimited.resetsAt ? new Date(rateLimited.resetsAt * 1000).toLocaleTimeString() : "later";
          text = `Claude rate limited (${rateLimited.rateLimitType ?? "unknown"}), resets at ${at}`;
        }
        errorMessage(text);
      }
    }
    rateLimited = undefined;
    // idle is the authoritative end of turn where the CLI reports it; a result
    // may also cover several merged steers, so only fall back to it otherwise
    if (!sawState) finish();
  }

  function handle(m: any) {
    switch (m?.type) {
      case "system":
        if (m.subtype === "init" && m.session_id && m.session_id !== sessionId) {
          sessionId = m.session_id;
          opts.store.set(sessionId);
        } else if (m.subtype === "session_state_changed") {
          sawState = true;
          if (m.state === "idle") finish();
        }
        break;
      case "stream_event":
        if (!m.parent_tool_use_id) onStream(m.event);
        break;
      case "assistant":
        if (!m.parent_tool_use_id) onAssistant(m);
        break;
      case "user":
        if (!m.parent_tool_use_id) onUser(m);
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
    const options: Options = {
      includePartialMessages: true,
      permissionMode: "bypassPermissions",
      allowDangerouslySkipPermissions: true,
      systemPrompt: { type: "preset", preset: "claude_code" },
      settingSources: [],
      cwd: opts.cwd ?? process.cwd(),
      model,
      extraArgs: { "thinking-display": "summarized" },
      env: claudeEnv(),
      ...(effort ? { effort } : {}),
      ...(sessionId ? { resume: sessionId } : {}),
    };
    input = stream;
    const mine = (q = run({ prompt: stream, options }));
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
        // the child is gone; the next prompt starts a new query with resume
        q = undefined;
        input = undefined;
        if (!disposed) finish();
      }
    })();
  }

  async function send(text: string, images?: Image[]) {
    if (!q) start();
    const content: any = images?.length
      ? [
          ...images.map((i) => ({ type: "image", source: { type: "base64", media_type: i.mimeType, data: i.data } })),
          { type: "text", text },
        ]
      : text;
    if (!running) {
      running = true;
      emit({ type: "agent_start" });
    }
    input!.push({ type: "user", message: { role: "user", content }, parent_tool_use_id: null });
  }

  return {
    isClaude: true as const,
    /** Claude Code runs its own tools under bypassPermissions; the TUI's gate has nothing to wrap. */
    agent: { beforeToolCall: undefined as any },
    get messages(): any[] {
      return [];
    },
    get model() {
      return { provider: "anthropic", id: model };
    },
    subscribe(l: Listener) {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    async prompt(text: string, o?: { images?: Image[] }) {
      await send(text, o?.images);
    },
    /** Pushed straight into the live input; Claude Code takes it at its next boundary. */
    async steer(text: string, images?: Image[]) {
      // clearQueue cannot recall a push, so a re-queue of one already sent is a no-op
      if (steering.includes(text)) return;
      steering.push(text);
      queueUpdate();
      await send(text, images);
    },
    /** Held here until the turn ends, then sent as its own prompt. */
    async followUp(text: string, images?: Image[]) {
      if (!running) return send(text, images);
      followUps.push({ text, images });
      queueUpdate();
    },
    clearQueue() {
      followUps.length = 0;
      queueUpdate();
    },
    async abort() {
      followUps.length = 0;
      if (!running || !q) return;
      aborting = true;
      await q.interrupt().catch(() => {});
    },
    async setModel(m: { id: string }) {
      model = m.id;
      await q?.setModel(m.id).catch(() => {});
    },
    /**
     * Live via applyFlagSettings (effortLevel accepts 'max'); a dead child
     * picks it up from the options of its replacement.
     */
    setThinkingLevel(level: Level) {
      effort = effortFor(level);
      void q?.applyFlagSettings({ effortLevel: effort ?? null }).catch(() => {});
    },
    /** Claude Code compacts on its own; nothing to switch from here. */
    setAutoCompactionEnabled(_on: boolean) {},
    dispose() {
      disposed = true;
      input?.close();
      q?.close?.();
      listeners.clear();
    },
  };
}

export type ClaudeSession = ReturnType<typeof createClaudeSession>;
