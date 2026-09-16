import { spawn, type ChildProcess } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { childTraceEnv } from "./tracing.ts";

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
 * workspace's tool server, plus that machine's own packages. `--tools` is a
 * strict allow-list over EXTENSION tools too, so the kl_pkg_* four have to be
 * named here or `kloudlite.ts` registers them into a session that cannot call them.
 */
export const WORKSPACE_TOOLS = "read,write,edit,bash,grep,find,ls,kl_pkg_list,kl_pkg_add,kl_pkg_rm,kl_pkg_update";

/** `tools`: the workspace whose tool server runs this session's tools. */
export type ChildOpts = { dir: string; file?: string; fork?: string; model: string; bin?: string; extDir?: string; cwd?: string; tools?: string };

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

  start(): void {
    if (this.child) return;
    const o = this.opts;
    const bin = o.bin ?? process.env.HARNESS_PI_BIN ?? path.join(HARNESS, "node_modules", ".bin", "pi");
    // The image installs the whole harness tree at /opt/harness, so the relative defaults resolve there; the env names another layout.
    const extDir = o.extDir ?? process.env.HARNESS_PI_EXT_DIR ?? path.join(HARNESS, "pi");
    // A workspace session loads its own tools, plus `kloudlite.ts` for that machine's own packages.
    // A bench session loads the platform and NOTHING that runs here: `--no-builtin-tools` takes the
    // built-in read/write/bash away and background.ts/process.ts are not loaded, because a bench
    // session has no hands in the bench pod at all (owner, 2026-09-17) — work for a workspace is
    // queued into that workspace's own session instead.
    // The btw fork answers one question from the transcript it forked: `--no-tools`, no extensions.
    const exts = o.fork ? [] : (o.tools ? ["workspace-tools.ts", "kloudlite.ts"] : ["kloudlite.ts"]).flatMap((f) => ["-e", path.join(extDir, f)]);
    const args = ["--mode", "rpc", "--model", o.model, "--session-dir", o.dir, ...exts, ...(o.file ? ["--session", o.file] : []), ...(o.fork ? ["--fork", o.fork, "--no-tools"] : []), ...(o.tools ? ["--tools", WORKSPACE_TOOLS] : []), ...(o.fork || o.tools ? [] : ["--no-builtin-tools"])];
    // KL_TEAM rides in from the bench's own env; the extension asks /v1 for the address, so nothing secret goes in argv.
    // The trace of the request that started this child; every tool call of its life joins it.
    // ponytail: one waterfall per child lifetime, unbounded; `workspace-tools.ts` stops sending it
    // after `TRACE_MAX_AGE_S`. The upgrade is a context refreshed per prompt (a field in pi's RPC)
    // or re-spawning the child's env when it goes idle.
    // KL_SESSION is how a tool call names the session it came from when it asks the bench for something.
    const env = { ...process.env, KL_SESSION: this.id, ...(o.tools ? { KL_TOOLS_WORKSPACE: o.tools } : {}), ...childTraceEnv() };
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
