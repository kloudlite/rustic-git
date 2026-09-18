import { spawn, type ChildProcess } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { childTraceEnv } from "./tracing.ts";
import { TOOLS } from "../../pi/catalog.ts";
import type { Triple } from "./defaults.ts";

/**
 * One session's pi, in RPC mode: JSONL over stdio. Framing is strict LF; Node's
 * readline also splits on U+2028/2029, which are legal inside JSON strings.
 * The session file lives in the bench folder (`--session-dir`), so nothing
 * here remembers anything — reopening a session is `--session <file>`.
 */
export type PiEvent = Record<string, unknown> & { type: string; id?: string };
const HARNESS = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

/**
 * A workspace or ephemeral session's tools: its own files and shell, on that
 * workspace's tool server, plus that machine's own packages and its space's
 * environment. `--tools` is a strict allow-list over EXTENSION tools too, so
 * every one `kloudlite.ts` registers in workspace mode has to be named here, or
 * it is registered into a session that cannot call it.
 */
/**
 * A workspace session's tools are no longer an allow-list: `tool_search` turns a deferred tool on
 * at runtime, and `--tools` would then admit a tool the extension had just activated only if this
 * string had been kept equal to the whole catalogue by hand. `--no-builtin-tools` alone leaves
 * exactly the extensions' tools, which is the right set for both kinds of session.
 */
export const WORKSPACE_TOOLS = "";

/** The built-in tools `--no-builtin-tools` takes away, and the same seven names `workspace-tools.ts`
 *  registers in their place — running on a tool server, not here. Only `tools()` reads these. */
const PI_BUILTINS = ["read", "write", "edit", "bash", "grep", "find", "ls"];
/** `process` has no built-in counterpart: long-running commands exist only on a tool server. */
/** What `workspace-tools.ts` registers — the machine's own hands, catalogue entries included. */
// What only a session WITH a machine can run. `kl_images` is a registry read and `kl_container_*`
// are an ask from the bench (`imageTools`), so those names exist in both modes and differ only in
// where the work happens.
export const IDE_TOOLS = [...PI_BUILTINS, "process", "kl_repo_clone", "report"];

/** `tools`: the workspace whose tool server runs this session's tools. */
/** `info`: a READ-ONLY fork that answers one question about a workspace, on that workspace's tool server. */
export const INFO_TOOLS = "read,grep,find,ls";
/** One row of `get_available_models`, reduced to what the picker draws. */
export type ModelInfo = { id: string; name: string; provider: string; thinking: boolean; effort: boolean };

export type ChildOpts = { dir: string; file?: string; fork?: string; model: string; bin?: string; extDir?: string; cwd?: string; tools?: string; tree?: string; ephemeral?: boolean; info?: boolean; thinking?: string; effort?: string };

export class RpcChild {
  readonly id: string;
  private opts: ChildOpts;
  private onEvent: (ev: PiEvent) => void;
  private child?: ChildProcess;
  private buf = "";
  private seq = 0;
  private waiting = new Map<string, { resolve: (r: PiEvent) => void; reject: (e: Error) => void }>();
  /** False until pi has answered once; commands sent before that are held rather than lost. */
  private ready = false;
  private pending: string[] = [];

  constructor(id: string, opts: ChildOpts, onEvent: (ev: PiEvent) => void) {
    this.id = id;
    this.opts = opts;
    this.onEvent = onEvent;
  }

  running(): boolean {
    return !!this.child;
  }

  /**
   * The child's whole argv but the binary — one place, because `tools()` answers what this session
   * can call by READING it. A probe that asserts a bench session has no `bash` is then asserting
   * about the process that runs, not about a second copy of the rules.
   */
  private args(extDir: string): string[] {
    const o = this.opts;
    // Every kind but the fork loads `workspace-tools.ts`; what differs is whose machine it points
    // at. A workspace session names its workspace (`KL_TOOLS_WORKSPACE`, address from /v1); a BENCH
    // session names none and registers no hands at all (spec §3.1) — reaching a workspace is `ask`,
    // a queue, never a tool call.
    //
    // `--no-builtin-tools` stays either way: a built-in read or bash would run in the BENCH
    // container, which is nobody's machine. It leaves exactly the extensions' tools, which for a
    // bench session is already the whole right set — so there is no `--tools` allow-list to keep
    // equal to the catalogue by hand.
    // The btw fork answers one question from the transcript it forked: `--no-tools`, and
    // `kloudlite.ts` loaded in fork mode purely so it is told what it is and registers nothing.
    // An INFO fork keeps the workspace's own hands — read-only ones — because the question is about
    // that workspace's files; an ordinary btw fork has none at all.
    const exts = (o.fork && !o.info ? ["kloudlite.ts"] : ["workspace-tools.ts", "kloudlite.ts"]).flatMap((f) => ["-e", path.join(extDir, f)]);
    return ["--mode", "rpc", "--model", o.model, "--session-dir", o.dir, ...exts, ...(o.file ? ["--session", o.file] : []), ...(o.fork ? ["--fork", o.fork, ...(o.info ? ["--tools", INFO_TOOLS] : ["--no-tools"])] : []), ...(o.info && !o.fork ? ["--tools", INFO_TOOLS] : []), ...(o.fork && !o.info ? [] : ["--no-builtin-tools"])];
  }

  /**
   * Every tool name this session can call, read off the argv above: `--no-tools` is none,
   * `--tools` is exactly its allow-list, and otherwise it is the extension's catalogue plus pi's
   * builtins — which `--no-builtin-tools` removes. Nothing here asks the child, because pi's RPC
   * has no tool listing; what it does have is these flags, and they are what decide.
   */
  tools(extDir = this.opts.extDir ?? process.env.HARNESS_PI_EXT_DIR ?? path.join(HARNESS, "pi")): string[] {
    const a = this.args(extDir);
    if (a.includes("--no-tools")) return [];
    // Not the builtins: the same seven NAMES, registered by `workspace-tools.ts` and run on a tool
    // server. `--no-builtin-tools` removes pi's own; these stay.
    // `workspace-tools.ts` registers nothing without a workspace to point at, which is a bench
    // session exactly (spec §3.1): the extension is loaded so it is told what it is, not for hands.
    const ide = a.some((x) => x.endsWith("workspace-tools.ts")) && (this.opts.tools || process.env.KL_TOOLS_ADDRESS) ? IDE_TOOLS : [];
    // `kloudlite.ts` never registers the entries that RUN on a machine — those are
    // `workspace-tools.ts`'s — so a session with no machine has neither half of them.
    const ext = a.some((x) => x.endsWith("kloudlite.ts")) ? TOOLS.map((t) => t.name).filter((n) => ide.length || !IDE_TOOLS.includes(n)) : [];
    return [...(a.includes("--no-builtin-tools") ? [] : PI_BUILTINS), ...ide, ...ext];
  }

  /**
   * `tools()` plus the two facts that say WHERE they run: the tool-server address this session's
   * child is handed, and whether pi's own builtins are on. A bench session has NO address and no
   * builtins: nothing it can call runs anywhere near the bench pod, which is what the fleet probe
   * checks.
   */
  hands(): { tools: string[]; toolsAddress?: string; builtinTools: boolean } {
    const o = this.opts;
    const extDir = o.extDir ?? process.env.HARNESS_PI_EXT_DIR ?? path.join(HARNESS, "pi");
    return {
      tools: this.tools(extDir),
      // A bench session has no tool server of its own (spec §3.1); a workspace session resolves
      // its address from /v1 when its child starts.
      toolsAddress: undefined,
      builtinTools: !this.args(extDir).includes("--no-builtin-tools"),
    };
  }

  start(): void {
    if (this.child) return;
    this.ready = false;
    this.pending.length = 0;
    const o = this.opts;
    const bin = o.bin ?? process.env.HARNESS_PI_BIN ?? path.join(HARNESS, "node_modules", ".bin", "pi");
    // The image installs the whole harness tree at /opt/harness, so the relative defaults resolve there; the env names another layout.
    const extDir = o.extDir ?? process.env.HARNESS_PI_EXT_DIR ?? path.join(HARNESS, "pi");
    const args = this.args(extDir);
    // KL_TEAM rides in from the bench's own env; the extension asks /v1 for the address, so nothing secret goes in argv.
    // The trace of the request that started this child; every tool call of its life joins it.
    // ponytail: one waterfall per child lifetime, unbounded; `workspace-tools.ts` stops sending it
    // after `TRACE_MAX_AGE_S`. The upgrade is a context refreshed per prompt (a field in pi's RPC)
    // or re-spawning the child's env when it goes idle.
    // KL_SESSION is how a tool call names the session it came from when it asks the bench for something.
    // Which machine this session's hands are on: a WORKSPACE session's own workspace, and nothing
    // at all for a bench session. It was handed its own pod's loopback tool server, which is the
    // "own machine is the default target" rule that died with spec §3.1 — the sessions container
    // has no tool server, so the model was told to start `bench-…` to make a build work
    // (owner, 2026-09-18).
    const env = { ...process.env, KL_SESSION: this.id, ...(o.effort ? { PI_EFFORT: o.effort } : {}), ...(o.fork || o.info ? { KL_FORK: "1" } : {}), ...(o.ephemeral ? { KL_EPHEMERAL: "1" } : {}),
      ...(o.tools ? { KL_TOOLS_WORKSPACE: o.tools, KL_WORKSPACE_ID: o.tools } : {}),
      // Which TREE of that workspace this session's hands act on. The extension pins it onto every
      // call rather than trusting the model to pass it (spec §4.4).
      ...(o.tree ? { KL_TREE: o.tree } : {}), ...childTraceEnv() };
    const c2 = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"], env, cwd: o.cwd ?? process.env.HOME });
    const child = c2;
    this.child = child;
    child.stdout!.on("data", (d: Buffer) => this.feed(d.toString("utf8")));
    let errTail = "";
    child.stderr!.on("data", (d: Buffer) => {
      const t = d.toString("utf8");
      errTail = (errTail + t).slice(-2000);
      this.onEvent({ type: "stderr", text: t });
    });
    child.on("exit", (code) => {
      this.child = undefined;
      this.ready = false;
      this.pending.length = 0;
      const err = new Error(`pi exited (${code})`);
      for (const w of this.waiting.values()) w.reject(err);
      this.waiting.clear();
      this.onEvent({ type: "exit", code, stderr: errTail.trim().split("\n").filter((l) => l.trim()).slice(-3).join(" · ") });
    });
    // The child primes ITSELF: every kind of child gets one `get_state`, so the buffer above always
    // has something that proves readiness. A fork is created directly rather than through `open()`,
    // and waiting on a `get_state` nobody sent would hold its commands forever.
    c2.stdin!.on("error", (error) => this.rejectWaiting(error instanceof Error ? error : new Error(String(error))));
    if (!this.ready) this.write(JSON.stringify({ type: "get_state", id: `c${++this.seq}` }) + "\n");
    this.onEvent({ type: "started", host: "bench", model: o.model, resumed: !!o.file, forked: !!o.fork });
  }

  /** Resolves once pi has exited; one still up after timeoutMs is SIGKILLed, so no stop leaves a pi behind. */
  stop(timeoutMs = 5_000): Promise<void> {
    const c = this.child;
    if (!c || c.exitCode !== null || c.signalCode !== null) return Promise.resolve();
    return new Promise((resolve) => {
      const t = setTimeout(() => (c.kill("SIGKILL"), resolve()), timeoutMs).unref();
      c.once("exit", () => (clearTimeout(t), resolve()));
      c.kill();
    });
  }

  /**
   * The session's triple, sent to a live child. `set_model` takes `{provider, modelId}` and nothing
   * else — the locked pi (0.85.1) has no effort option on it and no `capabilities.effort` on a
   * model; effort is carried to the extension in the spawn env (`PI_EFFORT`) instead, so it is
   * applied at the next start rather than mid-session. A model pi does not know is an error
   * response, not a throw: the person picked from a list a restarted bench may no longer offer.
   */
  async applyTriple(t: Triple): Promise<void> {
    if (!this.running()) return;
    if (t.model) {
      const at = t.model.indexOf("/");
      if (at > 0) await this.send({ type: "set_model", provider: t.model.slice(0, at), modelId: t.model.slice(at + 1) }).catch(() => undefined);
    }
    if (t.thinking) await this.send({ type: "set_thinking_level", level: t.thinking }).catch(() => undefined);
  }

  /**
   * What this child's pi can be set to. `thinking` is pi's own `reasoning`; `effort` is a level of
   * it this model actually maps (`thinkingLevelMap` marks an unsupported one null), which is what
   * decides whether the picker shows an effort row at all (spec §1.1: never a knob the model
   * cannot take).
   */
  async models(): Promise<ModelInfo[]> {
    if (!this.running()) return [];
    const ev = await this.send({ type: "get_available_models" }).catch(() => undefined);
    const rows = (ev?.data as { models?: Record<string, unknown>[] } | undefined)?.models ?? [];
    return rows.map((m) => {
      const map = (m.thinkingLevelMap ?? {}) as Record<string, unknown>;
      return { id: String(m.id), name: String(m.name ?? m.id), provider: String(m.provider ?? ""), thinking: m.reasoning === true, effort: m.reasoning === true && Object.values(map).some((v) => v !== null && v !== undefined) };
    });
  }

  /**
   * A prompt sent while the child is still SPAWNING was dropped without a trace: written to stdin
   * before pi had a session to answer with, it left no queue row and no error, and the caller's
   * promise resolved empty (api-test-report D2). A warm child kept order correctly, which is what
   * isolated the window to the cold start.
   *
   * Everything is queued until pi answers its first command — `open()` sends `get_state` at spawn,
   * so the wait is one round trip — and then flushed in arrival order.
   */
  send(cmd: Record<string, unknown>): Promise<PiEvent> {
    const c = this.child;
    if (!c) return Promise.reject(new Error("pi is not running"));
    const id = `c${++this.seq}`;
    return new Promise((resolve, reject) => {
      this.waiting.set(id, { resolve, reject });
      const line = JSON.stringify({ ...cmd, id }) + "\n";
      // `get_state` is what proves the child is up, so it goes first and never waits on itself.
      if (this.ready || cmd.type === "get_state") {
        if (!this.write(line)) this.rejectWaiting(new Error("pi stdin is closed"));
        return;
      }
      this.pending.push(line);
    });
  }

  private write(line: string): boolean {
    const stdin = this.child?.stdin;
    if (!stdin || stdin.destroyed) return false;
    try {
      stdin.write(line);
      return true;
    } catch {
      return false;
    }
  }

  private rejectWaiting(error: Error): void {
    for (const w of this.waiting.values()) w.reject(error);
    this.waiting.clear();
  }

  /** Whatever arrived before pi could answer, in the order it was sent. */
  private flush(): void {
    const c = this.child;
    if (!c) return;
    for (const line of this.pending.splice(0)) {
      if (!this.write(line)) {
        this.rejectWaiting(new Error("pi stdin is closed"));
        break;
      }
    }
  }

  private feed(chunk: string) {
    this.buf += chunk;
    let at: number;
    while ((at = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, at).replace(/\r$/, "");
      this.buf = this.buf.slice(at + 1);
      if (!line) continue;
      let ev: PiEvent;
      try {
        ev = JSON.parse(line) as PiEvent;
      } catch {
        this.onEvent({ type: "stderr", text: line });
        continue;
      }
      // pi has answered: it has a session and will read what follows. Anything held goes now.
      if (ev.type === "response" && !this.ready) {
        this.ready = true;
        this.flush();
      }
      const w = ev.type === "response" && ev.id ? this.waiting.get(ev.id) : undefined;
      if (w) {
        this.waiting.delete(ev.id!);
        w.resolve(ev);
      }
      this.onEvent(ev);
    }
  }
}
