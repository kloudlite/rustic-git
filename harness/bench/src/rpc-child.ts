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
/**
 * The bench's own workspace container's tool server. The two containers of a bench pod share a
 * network namespace, so this is the same loopback address `pty.ts` splices a bench-scope shell to.
 */
export const BENCH_TOOLS = "127.0.0.1:7788";
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
export const IDE_TOOLS = [...PI_BUILTINS, "process", "kl_repo_clone", "kl_container_build", "kl_container_push", "kl_images"];

/** `tools`: the workspace whose tool server runs this session's tools. */
/** `info`: a READ-ONLY fork that answers one question about a workspace, on that workspace's tool server. */
export const INFO_TOOLS = "read,grep,find,ls";
/** One row of `get_available_models`, reduced to what the picker draws. */
export type ModelInfo = { id: string; name: string; provider: string; thinking: boolean; effort: boolean };

export type ChildOpts = { dir: string; file?: string; fork?: string; model: string; bin?: string; extDir?: string; cwd?: string; tools?: string; ephemeral?: boolean; info?: boolean; ownWorkspace?: boolean; thinking?: string; effort?: string };

export class RpcChild {
  readonly id: string;
  private opts: ChildOpts;
  private onEvent: (ev: PiEvent) => void;
  private child?: ChildProcess;
  private buf = "";
  private seq = 0;
  private waiting = new Map<string, { resolve: (r: PiEvent) => void; reject: (e: Error) => void }>();

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
    // at. A workspace session names its workspace (`KL_TOOLS_WORKSPACE`, address from /v1); a bench
    // session is handed `BENCH_TOOLS`, its OWN workspace container's tool server on loopback, so
    // its hands are its own machine's and no other's — reaching a different workspace is
    // `kl_workspace_ask`, a queue, never a tool call (owner, 2026-09-17).
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
    const ide = a.some((x) => x.endsWith("workspace-tools.ts")) ? IDE_TOOLS : [];
    const ext = a.some((x) => x.endsWith("kloudlite.ts")) ? TOOLS.map((t) => t.name) : [];
    return [...(a.includes("--no-builtin-tools") ? [] : PI_BUILTINS), ...ide, ...ext];
  }

  /**
   * `tools()` plus the two facts that say WHERE they run: the tool-server address this session's
   * child is handed, and whether pi's own builtins are on. A bench session's hands are its own
   * workspace container's (`BENCH_TOOLS`) with the builtins off — the tool NAMES alone cannot tell
   * that from a session running them in the bench container, which is what the fleet probe checks.
   */
  hands(): { tools: string[]; toolsAddress?: string; builtinTools: boolean } {
    const o = this.opts;
    const extDir = o.extDir ?? process.env.HARNESS_PI_EXT_DIR ?? path.join(HARNESS, "pi");
    return {
      tools: this.tools(extDir),
      // The same condition the spawn env below is built from: a workspace session resolves its
      // address from /v1 instead, and a fork has no tools at all.
      toolsAddress: o.fork || o.tools ? undefined : BENCH_TOOLS,
      builtinTools: !this.args(extDir).includes("--no-builtin-tools"),
    };
  }

  start(): void {
    if (this.child) return;
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
    const env = { ...process.env, KL_SESSION: this.id, ...(o.effort ? { PI_EFFORT: o.effort } : {}), ...(o.fork || o.info ? { KL_FORK: "1" } : {}), ...(o.ephemeral ? { KL_EPHEMERAL: "1" } : {}), ...(o.fork || o.tools ? {} : { KL_TOOLS_ADDRESS: BENCH_TOOLS }), // Which machine this session's hands are on, and whose packages `kl_pkg_*` act on.
      ...(o.tools ? { KL_TOOLS_WORKSPACE: o.tools, KL_WORKSPACE_ID: o.tools } : {}),
      ...(o.ownWorkspace ? { KL_TOOLS_ADDRESS: BENCH_TOOLS } : {}), ...childTraceEnv() };
    const child = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"], env, cwd: o.cwd ?? process.env.HOME });
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
      const err = new Error(`pi exited (${code})`);
      for (const w of this.waiting.values()) w.reject(err);
      this.waiting.clear();
      this.onEvent({ type: "exit", code, stderr: errTail.trim().split("\n").filter((l) => l.trim()).slice(-3).join(" · ") });
    });
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

  send(cmd: Record<string, unknown>): Promise<PiEvent> {
    const c = this.child;
    if (!c) return Promise.reject(new Error("pi is not running"));
    const id = `c${++this.seq}`;
    return new Promise((resolve, reject) => {
      this.waiting.set(id, { resolve, reject });
      c.stdin!.write(JSON.stringify({ ...cmd, id }) + "\n");
    });
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
      const w = ev.type === "response" && ev.id ? this.waiting.get(ev.id) : undefined;
      if (w) {
        this.waiting.delete(ev.id!);
        w.resolve(ev);
      }
      this.onEvent(ev);
    }
  }
}
