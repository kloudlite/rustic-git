import { spawn, type ChildProcess } from "node:child_process";
import { Type } from "typebox";
import { StringEnum } from "@earendil-works/pi-ai";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

/**
 * Long-lived processes for the bench: a dev server, a watcher, a tunnel —
 * things a command starts and walks away from. `process start` detaches one
 * (own process group, a 64 KiB ring of its output), `logs` reads the ring,
 * `stop` ends it, `list` says what runs. The harness watches them through the
 * one fire-and-forget channel RPC mode gives an extension — `setWidget` —
 * which carries a JSON snapshot of every process every second while any runs,
 * and the harness's `/proc-stop <id>` reaches the same table as `stop`.
 */
type Proc = { id: string; name: string; command: string; proc: ChildProcess; started: number; ended?: number; code?: number | null; out: string[]; bytes: number };

const KEEP = 64 * 1024;
const HOLD = 60_000; // an exited process stays listed a minute, so its end is seen

export default function (pi: ExtensionAPI) {
  const procs = new Map<string, Proc>();
  let seq = 0;
  let ui: ExtensionContext["ui"] | undefined;
  let timer: ReturnType<typeof setInterval> | undefined;

  const tail = (p: Proc, lines = 200) => p.out.join("").split("\n").slice(-lines).join("\n");
  const push = (p: Proc, s: string) => {
    p.out.push(s);
    p.bytes += s.length;
    while (p.bytes > KEEP && p.out.length > 1) p.bytes -= p.out.shift()!.length;
  };
  const kill = (p: Proc, sig: NodeJS.Signals) => {
    try {
      if (p.proc.pid) process.kill(-p.proc.pid, sig);
    } catch {
      /* already gone */
    }
  };
  // The pid lets harness-bench tell, after its own restart, a process that
  // still runs from one that went with the old pod.
  const snapshot = () =>
    [...procs.values()].map((p) => ({ id: p.id, name: p.name, command: p.command, pid: p.proc.pid, started: p.started, ended: p.ended, code: p.code, tail: tail(p, 40) }));
  const publish = () => {
    for (const [id, p] of procs) if (p.ended && Date.now() - p.ended > HOLD) procs.delete(id);
    ui?.setWidget("harness:procs", [JSON.stringify(snapshot())]);
    if (!procs.size && timer) (clearInterval(timer), (timer = undefined));
  };
  const watch = () => {
    if (!timer) timer = setInterval(publish, 1000);
    publish();
  };

  const start = (command: string, name: string | undefined, ctx: ExtensionContext) => {
    ui = ctx.ui;
    const id = `p${++seq}`;
    const proc = spawn("bash", ["-c", command], { cwd: ctx.cwd, env: process.env, stdio: ["ignore", "pipe", "pipe"], detached: true });
    const p: Proc = { id, name: name || command.split(/\s+/).slice(0, 2).join(" "), command, proc, started: Date.now(), out: [], bytes: 0 };
    procs.set(id, p);
    const onData = (d: Buffer) => push(p, d.toString("utf8"));
    proc.stdout!.on("data", onData);
    proc.stderr!.on("data", onData);
    proc.on("close", (code) => {
      p.ended = Date.now();
      p.code = code;
      publish();
    });
    watch();
    return p;
  };

  pi.registerTool({
    name: "process",
    label: "process",
    description:
      "Manage long-running processes (dev servers, watchers). start: launch `command` detached and return its id; logs: the last lines of a process's output; stop: end it; list: everything running. Use this instead of bash for anything that does not exit on its own.",
    parameters: Type.Object({
      action: StringEnum(["start", "logs", "stop", "list"] as const),
      command: Type.Optional(Type.String({ description: "start: the command to run" })),
      name: Type.Optional(Type.String({ description: "start: a short name for it" })),
      id: Type.Optional(Type.String({ description: "logs/stop: the process id" })),
      lines: Type.Optional(Type.Number({ description: "logs: how many trailing lines (default 100)" })),
    }),
    async execute(_id, a, _signal, _update, ctx) {
      const text = (t: string) => ({ content: [{ type: "text" as const, text: t }] });
      if (a.action === "start") {
        if (!a.command) return { ...text("start needs a command"), isError: true };
        const p = start(a.command, a.name, ctx);
        await new Promise((r) => setTimeout(r, 1500));
        return text(`Started ${p.id} (${p.name}): ${p.command}${p.ended ? `\nIt exited already with code ${p.code}.` : ""}\n\nFirst output:\n${tail(p, 20) || "(none yet)"}`);
      }
      const p = a.id ? procs.get(a.id) : undefined;
      if (a.action === "list") return text(procs.size ? [...procs.values()].map((x) => `${x.id}  ${x.ended ? `exited ${x.code}` : "running"}  ${x.name}: ${x.command}`).join("\n") : "No processes.");
      if (!p) return { ...text(`No process ${a.id ?? ""}; use list.`), isError: true };
      if (a.action === "logs") return text(tail(p, a.lines ?? 100) || "(no output)");
      kill(p, "SIGTERM");
      setTimeout(() => kill(p, "SIGKILL"), 3000);
      return text(`Stopping ${p.id} (${p.name}).`);
    },
  });

  pi.registerCommand("proc-stop", {
    description: "Stop a process by id (used by the harness)",
    handler: async (args, ctx) => {
      const p = procs.get((args ?? "").trim());
      if (!p) return void ctx.ui.notify(`No process ${args}`, "warning");
      kill(p, "SIGTERM");
      setTimeout(() => kill(p, "SIGKILL"), 3000);
      ctx.ui.notify(`Stopping ${p.id}`, "info");
    },
  });
}
