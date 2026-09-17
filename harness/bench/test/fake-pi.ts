#!/usr/bin/env node
// A pi stand-in speaking just enough of `pi --mode rpc` for harness-bench's tests.
import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const FAKE = fileURLToPath(import.meta.url);

// Importing this module (for FAKE) must not also run it — it would attach a
// stdin listener in whatever process did the importing, hanging it forever.
if (process.argv[1] === FAKE) main();

function main() {
const argv = process.argv.slice(2);
const flag = (n: string) => (argv.includes(n) ? argv[argv.indexOf(n) + 1] : undefined);
const dir = flag("--session-dir") ?? ".";
const file = flag("--session") ?? path.join(dir, `fake-${process.pid}-${Date.now()}.jsonl`);
if (!fs.existsSync(file)) fs.writeFileSync(file, JSON.stringify({ type: "session", version: 3, id: path.basename(file, ".jsonl"), timestamp: new Date().toISOString(), cwd: process.cwd() }) + "\n");
// A test that wants this process's argv (e.g. to check `--tools`) sets this
// env var to a path; writing it here never touches the RPC stream, so it
// cannot shift the event order a real client's test asserts on.
if (process.env.FAKE_PI_ARGV_FILE) fs.writeFileSync(process.env.FAKE_PI_ARGV_FILE, JSON.stringify(argv));
const messages: unknown[] = [];
const queued: string[] = [];
const steering: string[] = [];
const out = (v: unknown) => process.stdout.write(JSON.stringify(v) + "\n");
let buf = "";
let turning = false;
process.stdin.on("data", (d) => {
  buf += d.toString();
  let at;
  while ((at = buf.indexOf("\n")) >= 0) {
    const cmd = JSON.parse(buf.slice(0, at));
    buf = buf.slice(at + 1);
    const ok = (data?: unknown) => out({ type: "response", id: cmd.id, command: cmd.type, success: true, data });
    if (cmd.type === "get_state") ok({ sessionFile: file, isStreaming: false, argv, tools: process.env.KL_TOOLS_WORKSPACE, team: process.env.KL_TEAM });
    else if (cmd.type === "get_messages") ok({ messages });
    else if (cmd.type === "abort") ok();
    // Enough of pi's queue for the harness's own triage: what is held, and putting it back.
    else if (cmd.type === "clear_queue") { const held = { steering: steering.splice(0), followUp: queued.splice(0) }; ok(held); }
    else if (cmd.type === "steer") { steering.push(String(cmd.message)); ok(); }
    // A follow-up DURING a turn is held, the way pi holds one; outside a turn it simply runs,
    // which is what a test of the harness's own queueing needs to see.
    else if (cmd.type === "follow_up" && turning) { queued.push(String(cmd.message)); ok(); }
    else if (cmd.type === "prompt" || cmd.type === "follow_up") {
      // endsWith, not equality: an ask arrives tagged, and a tagged "crash" is still a crash.
      if (String(cmd.message).endsWith("crash")) process.exit(3);
      ok();
      const asked = { role: "user", content: cmd.message, timestamp: Date.now() };
      messages.push(asked);
      turning = true;
      out({ type: "agent_start" });
      if (String(cmd.message).endsWith("hang")) return; // never answers: for a bounded-wait timeout test
      if (cmd.message === "task") { out({ type: "tool_execution_start", toolCallId: "t1", toolName: "bash", args: { command: "sleep 600" } }); return; } // a task left "running": no tool_execution_end
      if (cmd.message === "exchange") out({ type: "extension_ui_request", id: "w1", method: "setWidget", widgetKey: "harness:exchange", widgetLines: [JSON.stringify({ id: "e1", workspace: "api", dir: "out", text: "kl_workspace_start api", state: "sent" })] });
      if (cmd.message === "proc") {
        // A real child, so a reschedule drill can actually kill it and see the restart mark it lost.
        const child = spawn("sleep", ["600"]);
        out({ type: "extension_ui_request", id: "w2", method: "setWidget", widgetKey: "harness:procs", widgetLines: [JSON.stringify([{ id: "p1", name: "sleeper", command: "sleep 600", pid: child.pid, started: Date.now() }])] });
      }
      out({ type: "message_update", assistantMessageEvent: { type: "text_delta", delta: `echo ${cmd.message}` } });
      const answer = { role: "assistant", content: [{ type: "text", text: `echo ${cmd.message}` }], timestamp: Date.now() };
      messages.push(answer);
      // Real pi hands the run's own messages to agent_end (rpc.md) — the prompt that started it
      // among them, which is how the harness tells whose ask a turn answered.
      turning = false;
      out({ type: "agent_end", messages: [asked, answer] });
    } else out({ type: "response", id: cmd.id, command: cmd.type, success: false, error: `fake pi: ${cmd.type}` });
  }
});
}
