import { spawn, type ChildProcess } from "node:child_process";
import { Type } from "typebox";
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

/**
 * Background tasks for the bench, the way Claude Code's ctrl+b works: a bash
 * call that is taking long can be sent to the background — its tool result
 * returns at once, naming the task, and the command keeps running; when it
 * finishes, its output is delivered as a message so the model picks it up.
 *
 * The harness sends `/bg` (an extension command runs immediately, even while
 * the agent streams) and this file does the rest: it owns the bash tool so it
 * can hand a running process over instead of waiting on it.
 */
type Task = { n: number; command: string; proc: ChildProcess; out: string[]; bytes: number; done?: (r: { content: { type: "text"; text: string }[] }) => void };

const KEEP = 64 * 1024;

export default function (pi: ExtensionAPI) {
  const running = new Map<string, Task>();
  const backgrounded = new Map<number, Task>();
  let seq = 0;

  const kill = (t: Task, sig: NodeJS.Signals) => {
    try {
      if (t.proc.pid) process.kill(-t.proc.pid, sig);
    } catch {
      /* already gone */
    }
  };
  const tail = (t: Task) => t.out.join("").slice(-4000);
  const push = (t: Task, s: string) => {
    t.out.push(s);
    t.bytes += s.length;
    while (t.bytes > KEEP && t.out.length > 1) t.bytes -= t.out.shift()!.length;
  };

  pi.registerTool({
    name: "bash",
    label: "Bash",
    description:
      "Run a shell command in the working directory and return its output. Long-running commands may be sent to the background by the user; then the result names a task and the output arrives later as a message.",
    parameters: Type.Object({
      command: Type.String({ description: "The command to run" }),
      timeout: Type.Optional(Type.Number({ description: "Seconds before the command is killed" })),
    }),
    async execute(toolCallId, params, signal, onUpdate, ctx) {
      // Its own process group, so a cancel reaches the whole pipeline and not
      // only the shell — bash defers a signal while it waits on a child.
      const proc = spawn("bash", ["-c", params.command], { cwd: ctx.cwd, env: process.env, stdio: ["ignore", "pipe", "pipe"], detached: true });
      const t: Task = { n: 0, command: params.command, proc, out: [], bytes: 0 };
      running.set(toolCallId, t);
      const onData = (d: Buffer) => {
        push(t, d.toString("utf8"));
        // A backgrounded task's call has already been answered; streaming into
        // it now would be an update outside the run it belonged to.
        if (!t.n) onUpdate?.({ content: [{ type: "text", text: tail(t) }] });
      };
      proc.stdout!.on("data", onData);
      proc.stderr!.on("data", onData);
      const timer = params.timeout ? setTimeout(() => kill(t, "SIGKILL"), params.timeout * 1000) : undefined;
      signal?.addEventListener("abort", () => kill(t, "SIGTERM"));

      return new Promise((resolve) => {
        t.done = resolve;
        proc.on("close", (code) => {
          clearTimeout(timer);
          running.delete(toolCallId);
          const text = `${tail(t)}${code === 0 ? "" : `\n[exit ${code}]`}`.trim() || "(no output)";
          if (backgrounded.has(t.n)) {
            // Already answered; the model learns the outcome as a message.
            backgrounded.delete(t.n);
            pi.sendMessage(
              { customType: "background-task", content: `Background task #${t.n} finished (exit ${code}): ${t.command}\n\n${text}`, display: true },
              { deliverAs: "steer", triggerTurn: true },
            );
          } else resolve({ content: [{ type: "text", text }], isError: code !== 0 });
        });
      });
    },
  });

  pi.registerCommand("cancel", {
    description: "Kill a running or backgrounded command: /cancel <toolCallId> or /cancel #N",
    handler: async (args, ctx) => {
      const key = (args ?? "").trim();
      const t = key.startsWith("#") ? backgrounded.get(Number(key.slice(1))) : running.get(key);
      if (!t) return void ctx.ui.notify(`No task ${key}`, "warning");
      kill(t, "SIGTERM");
      setTimeout(() => kill(t, "SIGKILL"), 3000);
      ctx.ui.notify(`Cancelled ${key}`, "info");
    },
  });

  pi.registerCommand("bg", {
    description: "Send the running command to the background (ctrl+b in the harness)",
    handler: async (_args, ctx) => {
      const latest = [...running.values()].filter((t) => !t.n).pop();
      if (!latest) return void ctx.ui.notify("Nothing is running", "info");
      latest.n = ++seq;
      backgrounded.set(latest.n, latest);
      latest.done?.({ content: [{ type: "text", text: `Sent to the background as task #${latest.n}. Its output will be delivered as a message when it finishes; carry on with other work meanwhile.\n\nOutput so far:\n${tail(latest) || "(none)"}` }] });
      ctx.ui.notify(`Task #${latest.n} in the background`, "info");
    },
  });
}
