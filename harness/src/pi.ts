import { spawn, type ChildProcess } from "node:child_process";
import path from "node:path";
import fs from "node:fs";
import type { WebContents } from "electron";

/**
 * The bench runs on pi (pi.dev) in RPC mode: one process, JSONL over stdio,
 * events streamed back to the renderer as they happen. The same protocol
 * works wherever the process runs — locally for now, and over ssh when
 * HARNESS_PI_HOST names a machine, in which case pi and its sessions live
 * THERE (`~/.pi/agent/sessions` on that host) and the laptop is only a
 * screen for them. Nothing here holds a credential: pi reads its provider
 * key from the environment it starts in (DEEPSEEK_API_KEY and the like).
 *
 * Framing is strict LF-delimited JSON. Node's readline also splits on
 * U+2028/2029, which are legal inside JSON strings, so lines are cut by hand.
 */
export type PiEvent = Record<string, unknown> & { type: string; id?: string };

/** A side session: forked from a session file, and only allowed to look. */
export type Fork = { fork: string };
const READ_ONLY_TOOLS = "read,grep,find,ls,kl_workspaces,kl_workspace,kl_environments,kl_environment,kl_regions,kl_quota,kl_volumes,kl_builder,kl_whoami";

export class Pi {
  private child?: ChildProcess;
  private buf = "";
  private seq = 0;
  private waiting = new Map<string, (r: PiEvent) => void>();
  /** `memo` is where the bench's last session file is kept, so a relaunch
      resumes it; a fork has none — it is a copy that lives as long as its tab. */
  constructor(readonly id: string, private readonly sink: () => WebContents | undefined, private readonly memo?: string, private readonly opts?: Fork) {}

  private remembered(): string | undefined {
    if (!this.memo) return undefined;
    try {
      const v = fs.readFileSync(this.memo, "utf8").trim();
      return v && fs.existsSync(v) ? v : undefined;
    } catch {
      return undefined;
    }
  }
  remember(sessionFile: string) {
    if (!this.memo) return;
    try {
      fs.mkdirSync(path.dirname(this.memo), { recursive: true });
      fs.writeFileSync(this.memo, sessionFile);
    } catch {
      /* forgetting is not fatal; the session file itself is pi's */
    }
  }

  start(): void {
    if (this.child) return;
    const host = process.env.HARNESS_PI_HOST;
    const model = process.env.HARNESS_PI_MODEL ?? "deepseek/deepseek-v4-flash";
    const local = path.join(__dirname, "..", "node_modules", ".bin", "pi");
    const last = host ? undefined : this.remembered();
    // The harness's own extensions ride along: background tasks (ctrl+b) and
    // the platform's tools.
    const exts = ["background.ts", "process.ts", "kloudlite.ts"].flatMap((f) => ["-e", path.join(__dirname, "..", "pi", f)]);
    const args = ["--mode", "rpc", "--model", model, ...exts, ...(last ? ["--session", last] : []), ...(this.opts ? ["--fork", this.opts.fork, "--tools", READ_ONLY_TOOLS] : [])];
    this.child = host
      ? spawn("ssh", ["-T", host, "pi", ...args], { stdio: ["pipe", "pipe", "pipe"] })
      : spawn(local, args, { stdio: ["pipe", "pipe", "pipe"], env: process.env, cwd: process.env.HARNESS_PI_CWD ?? process.cwd() });
    this.child.stdout!.on("data", (d: Buffer) => this.feed(d.toString("utf8")));
    // The last of stderr rides with the exit: a process that dies at start
    // says why there, and nowhere else.
    let errTail = "";
    this.child.stderr!.on("data", (d: Buffer) => {
      const t = d.toString("utf8");
      errTail = (errTail + t).slice(-2000);
      this.emit({ type: "stderr", text: t });
    });
    this.child.on("exit", (code) => {
      this.emit({ type: "exit", code, stderr: errTail.trim().split("\n").filter((l) => l.trim()).slice(-3).join(" · ") });
      this.child = undefined;
    });
    this.emit({ type: "started", host: host ?? "local", model, resumed: !!last, forked: !!this.opts });
  }

  stop(): void {
    this.child?.kill();
    this.child = undefined;
  }

  send(cmd: Record<string, unknown>): Promise<PiEvent> {
    this.start();
    const id = `c${++this.seq}`;
    return new Promise((resolve) => {
      this.waiting.set(id, resolve);
      this.child!.stdin!.write(JSON.stringify({ id, ...cmd }) + "\n");
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
        this.emit({ type: "stderr", text: line });
        continue;
      }
      if (ev.type === "response" && ev.id && this.waiting.has(ev.id)) {
        this.waiting.get(ev.id)!(ev);
        this.waiting.delete(ev.id);
      }
      this.emit(ev);
    }
  }

  private emit(ev: PiEvent) {
    const wc = this.sink();
    if (wc && !wc.isDestroyed()) wc.send("pi:event", { ...ev, pi: this.id });
  }
}
