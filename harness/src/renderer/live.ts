import { createSignal } from "solid-js";
import { createStore, produce } from "solid-js/store";
import type { Message } from "./model";

/**
 * The bench thread as pi streams it. Events fold into the same `Message`
 * shape the fixtures use, so the transcript renders live and recorded threads
 * with one component: a prompt is a user message, text deltas grow one
 * assistant message, a tool call is an action row that resolves when the tool
 * ends. Nothing is kept here across restarts — pi's session file is the record.
 */
type Ev = Record<string, unknown> & { type: string };

/**
 * What the bench is running right now and what it sent to the background:
 * every tool call while it runs, and a backgrounded command until its output
 * comes back. The inspector lists these; the transcript only tells the story.
 */
export type Task = { id: string; session: string; n?: number; tool: string; arg: string; state: "running" | "background" | "done" | "failed" | "cancelled"; started: number; ended?: number; output: string };
const [tasks, setTasks] = createStore<Task[]>([]);
export { tasks };
const taskIndex = (id: string) => tasks.findIndex((t) => t.id === id);
/**
 * Long-lived processes the bench started, as the extension publishes them:
 * a JSON snapshot on the `harness:procs` widget every second while any runs.
 */
export type Proc = { id: string; session?: string; name: string; command: string; started: number; ended?: number; code?: number | null; tail: string };
const [procs, setProcs] = createStore<Proc[]>([]);
export { procs };
const [sessionCount, setSessionCount] = createSignal(1);
export { sessionCount, setSessionCount };
/** Sessions whose workspace messages were discarded with them; queues hide these. */
const [discarded, setDiscarded] = createSignal(new Set<string>());
export { discarded };
export function discard(id: string) {
  setDiscarded((d) => new Set(d).add(id));
}

export function stopProc(p: Proc) {
  void window.harness.pi({ type: "prompt", message: `/proc-stop ${p.id}` }, p.session ?? "bench");
}

export function cancel(t: Task) {
  const i = taskIndex(t.id);
  if (i >= 0) setTasks(i, { state: "cancelled" });
  void window.harness.pi({ type: "prompt", message: `/cancel ${t.n ? `#${t.n}` : t.id}` }, t.session);
}

const now = () => new Date().toTimeString().slice(0, 5);
const argOf = (name: string, args: Record<string, unknown>) =>
  name === "bash" ? String(args.command ?? "") : String(args.path ?? args.file_path ?? args.pattern ?? JSON.stringify(args)).slice(0, 200);
/** Tool names as Claude Code prints them, so a transcript reads the same in both. */
const TOOL: Record<string, string> = { bash: "Bash", read: "Read", write: "Write", edit: "Edit", grep: "Grep", glob: "Glob", ls: "List" };

/**
 * One thread's live state, keyed by the pi process behind it. The bench is
 * "bench"; a `/btw` side session is its own process and its own state. Only
 * the bench feeds the tasks and processes lists — a side session is read-only
 * and runs nothing worth watching.
 */
function makeThread(id: string) {
  // A fork (`btw-N`) is read-only and runs nothing worth watching; every
  // session feeds the shared tasks and processes lists, tagged by session.
  const bench = !id.startsWith("btw-");
  // A store, not a signal of arrays: a streamed delta changes one message's text
  // in place, so the transcript keeps every DOM node it has and only the text
  // node grows. Replacing the array on each delta re-created the rows, which is
  // what made the view flicker and the scroll jump.
  const [messages, setMessages] = createStore<Message[]>([]);
  const [busy, setBusy] = createSignal(false);
  const [status, setStatus] = createSignal<string>("not started");
  // False until pi has answered `get_messages` once: before that the thread is
  // unknown, not empty, and the pane must not flash the first-run intro.
  const [ready, setReady] = createSignal(false);

  /**
   * Images pasted into the composer, waiting for the next send. Each is numbered
   * once per session and named in the text as `[Image #N]` at the caret, the way
   * Claude Code does it, so a person can point at it in their own words.
   */
  const [attachments, setAttachments] = createSignal<Attachment[]>([]);
  function attach(file: File): Promise<number> {
    const n = ++imageSeq;
    return new Promise((resolve) => {
      const reader = new FileReader();
      reader.onload = () => {
        const url = reader.result as string;
        setAttachments((as) => [...as, { id: `img-${n}`, n, mimeType: file.type, data: url.split(",")[1] ?? "", url }]);
        resolve(n);
      };
      reader.readAsDataURL(file);
    });
  }
  function detach(id: string) {
    setAttachments((as) => as.filter((a) => a.id !== id));
  }
  function takeAttachments(): Attachment[] {
    const as = attachments();
    setAttachments([]);
    return as;
  }

  const push = (m: Message) => setMessages(produce((ms) => void ms.push({ ts: Date.now(), ...m })));


  /** A session pi already holds, replayed into the same shape the live events build. */
  function replay(raw: unknown[]) {
    const out: Message[] = [];
    const calls = new Map<string, number>();
    for (const m of raw as Record<string, unknown>[]) {
      const ts = typeof m.timestamp === "number" ? m.timestamp : undefined;
      const at = ts ? new Date(ts).toTimeString().slice(0, 5) : "";
      if (m.role === "user") {
        const c = m.content;
        const text = typeof c === "string" ? c : (c as { type: string; text?: string }[]).map((x) => x.text ?? "").join("");
        if (text.trim()) out.push({ role: "user", text, at, ts });
      } else if (m.role === "assistant") {
        for (const c of m.content as Record<string, unknown>[]) {
          if (c.type === "text" && (c.text as string).trim()) out.push({ role: "assistant", text: c.text as string, at, ts });
          if (c.type === "toolCall") {
            const name = c.name as string;
            const args = c.arguments as Record<string, unknown>;
            out.push({ role: "action", kind: name === "bash" ? "run" : "note", target: TOOL[name] ?? name, text: argOf(name, args), at, ts, output: "", tool: name, args });
            calls.set(c.id as string, out.length - 1);
          }
        }
      } else if (m.role === "toolResult") {
        const i = calls.get(m.toolCallId as string);
        if (i === undefined) continue;
        const content = (m.content as { type?: string; text?: string }[]) ?? [];
        const a = out[i] as Extract<Message, { role: "action" }>;
        out[i] = { ...a, ok: !(m.isError as boolean), output: content.map((x) => (x.type === "image" ? "(image)" : x.text ?? "")).join("").trim(), ms: ts && a.ts ? ts - a.ts : undefined };
      }
    }
    setMessages(produce((ms) => void ms.splice(0, ms.length, ...out)));
    open = -1;
    setReady(true);
  }

  /** A line from the harness itself, on the rail, the way a shell answers a builtin. */
  function note(text: string) {
    push({ role: "action", kind: "note", target: "harness", text: text.split("\n")[0], at: now(), ok: true, output: text.includes("\n") ? text : undefined });
  }

  // What is waiting on the bench: pi's own queue, as it reports it. A line the
  // person queued is shown here, not in the transcript, until pi delivers it.
  const [queue, setQueue] = createStore<{ text: string; how: "queue" | "steer" }[]>([]);
  function queued(text: string, how: "queue" | "steer") {
    setQueue(produce((q) => void q.push({ text, how })));
  }

  function sent(text: string, images: number[] = []) {
    push({ role: "user", text, at: now(), images });
  }

  /** The assistant message being streamed, by index into `messages`. */
  let open = -1;
  const tools = new Map<string, number>();

  function onEvent(ev: Ev) {
    switch (ev.type) {
      case "started":
        setStatus(`${ev.model as string} · ${ev.host as string}${ev.resumed ? " · resumed" : ""}${ev.forked ? " · read-only fork" : ""}`);
        return;
      case "exit":
        setStatus(`pi exited (${String(ev.code)})${ev.stderr ? `: ${String(ev.stderr).slice(0, 300)}` : ""}`);
        if (ev.stderr) note(`pi exited (${String(ev.code)}): ${String(ev.stderr)}`);
        setBusy(false);
        return;
      case "stderr":
        // Only a failure to start is worth surfacing; pi is chatty on stderr.
        if (/error|missing|api key|not found/i.test(ev.text as string)) setStatus((ev.text as string).trim().slice(0, 120));
        return;
      case "extension_ui_request": {
        if (!bench || ev.method !== "setWidget" || ev.widgetKey !== "harness:procs") return;
        const lines = ev.widgetLines as string[] | undefined;
        try {
          const next = (lines?.[0] ? (JSON.parse(lines[0]) as Proc[]) : []).map((p) => ({ ...p, session: id }));
          // Each session publishes only its own; the others' rows stay.
          setProcs(produce((ps) => void ps.splice(0, ps.length, ...ps.filter((p) => p.session !== id), ...next)));
        } catch {
          /* a widget line that is not ours */
        }
        return;
      }
      case "queue_update": {
        // pi is the truth: what it still holds stays; what it delivered is
        // echoed into the transcript as the prompt it became.
        const held = new Set([...((ev.steering as string[]) ?? []), ...((ev.followUp as string[]) ?? [])]);
        const gone = queue.filter((q) => !held.has(q.text));
        gone.forEach((q) => push({ role: "user", text: q.text, at: now() }));
        setQueue(produce((q) => void q.splice(0, q.length, ...q.filter((x) => held.has(x.text)))));
        return;
      }
      case "agent_start":
        setBusy(true);
        return;
      case "agent_end":
        setBusy(false);
        open = -1;
        // A finished call is not a task any more: keep the live ones and the
        // ones that ended in the last few seconds (the list lets them settle).
        if (bench) setTasks(produce((ts) => {
          const now = Date.now();
          const keep = ts.filter((t) => t.state === "running" || t.state === "background" || (t.ended && now - t.ended < 4000));
          ts.splice(0, ts.length, ...keep);
        }));
        return;
      case "message_update": {
        const d = ev.assistantMessageEvent as { type: string; delta?: string } | undefined;
        if (d?.type !== "text_delta" || !d.delta) return;
        if (open < 0) {
          push({ role: "assistant", text: d.delta, at: now() });
          open = messages.length - 1;
        } else {
          setMessages(open, "text" as never, ((t: string) => t + d.delta) as never);
        }
        return;
      }
      case "message_end": {
        open = -1;
        // A background task reporting in: shown as a note, the way the terminal
        // prints a job finishing.
        const m = ev.message as { role?: string; customType?: string; content?: string } | undefined;
        if (m?.role === "custom" && m.customType === "kl-login" && typeof m.content === "string") {
          push({ role: "action", kind: "note", target: "Kloudlite", text: m.content, at: now(), ok: true });
        }
        if (m?.role === "custom" && m.customType === "background-task" && typeof m.content === "string") {
          const [head, ...rest] = m.content.split("\n");
          const ok = !/exit [1-9]/.test(head);
          push({ role: "action", kind: "note", target: "Task", text: head.replace(/^Background task /, ""), at: now(), output: rest.join("\n").trim(), ok });
          const n = Number(/#(\d+)/.exec(head)?.[1]);
          const ti = bench ? tasks.findIndex((t) => t.n === n) : -1;
          if (ti >= 0) setTasks(ti, { state: tasks[ti].state === "cancelled" ? "cancelled" : ok ? "done" : "failed", ended: Date.now(), output: rest.join("\n").trim() });
        }
        return;
      }
      case "tool_execution_start": {
        const args = ev.args as Record<string, unknown>;
        const name = ev.toolName as string;
        push({ role: "action", kind: name === "bash" ? "run" : "note", target: TOOL[name] ?? name, text: argOf(name, args), at: now(), pending: true, tool: name, args });
        tools.set(ev.toolCallId as string, messages.length - 1);
        if (bench) setTasks(produce((ts) => void ts.push({ id: ev.toolCallId as string, session: id, tool: TOOL[name] ?? name, arg: argOf(name, args), state: "running", started: Date.now(), output: "" })));
        open = -1;
        return;
      }
      case "tool_execution_update": {
        const ti = bench ? taskIndex(ev.toolCallId as string) : -1;
        const partial = (ev.partialResult as { content?: { text?: string }[] } | undefined)?.content?.map((c) => c.text ?? "").join("") ?? "";
        if (ti >= 0) setTasks(ti, "output", partial);
        return;
      }
      case "tool_execution_end": {
        const i = tools.get(ev.toolCallId as string);
        if (i === undefined) return;
        const content = (ev.result as { content?: { type?: string; text?: string }[] } | undefined)?.content ?? [];
        const out = content.map((c) => (c.type === "image" ? "(image)" : c.text ?? "")).join("").trim();
        setMessages(i, { ok: !(ev.isError as boolean), output: out, pending: false, ms: Date.now() - ((messages[i] as { ts?: number }).ts ?? Date.now()) } as never);
        const bg = /^Sent to the background as task #(\d+)/.exec(out);
        const ti = bench ? taskIndex(ev.toolCallId as string) : -1;
        if (ti >= 0) {
          if (bg) setTasks(ti, { n: Number(bg[1]), state: "background" });
          else setTasks(ti, { state: tasks[ti].state === "cancelled" ? "cancelled" : ev.isError ? "failed" : "done", ended: Date.now(), output: out });
        }
        tools.delete(ev.toolCallId as string);
        return;
      }
    }
  }

  return { id, messages, busy, status, setStatus, ready, attachments, attach, detach, takeAttachments, replay, note, sent, queued, queue, onEvent };
}

export type Attachment = { id: string; n: number; mimeType: string; data: string; url: string };
let imageSeq = 0;

const threads = new Map<string, ReturnType<typeof makeThread>>();
/** The live state for a pi id; made on first use, idle until events arrive. */
export function thread(id: string) {
  let t = threads.get(id);
  if (!t) threads.set(id, (t = makeThread(id)));
  return t;
}
/** Every event says which process it came from; the bench is the default. */
export function onEvent(ev: Ev & { pi?: string }) {
  thread(typeof ev.pi === "string" && ev.pi ? ev.pi : "bench").onEvent(ev);
}

// The bench, under the names everything already reads.
const bench = thread("bench");
export const { messages, busy, status, ready, attachments, attach, detach, takeAttachments, replay, note, sent } = bench;
