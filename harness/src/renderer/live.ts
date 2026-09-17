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
export type Task = { id: string; session: string; n?: number; tool: string; arg: string; state: "running" | "background" | "done" | "failed" | "cancelled" | "lost"; started: number; ended?: number; output: string };
const [tasks, setTasks] = createStore<Task[]>([]);
export { tasks };
const taskIndex = (id: string) => tasks.findIndex((t) => t.id === id);
/**
 * Long-lived processes the bench started: the bench folds every session's
 * `harness:procs` widget into one table and sends it whole as `procs`. A row
 * found gone when the bench restarted is `lost`, with `ended` set.
 */
export type Proc = { id: string; session?: string; name: string; command: string; started: number; ended?: number; code?: number | null; tail: string; pid?: number; lost?: true };
const [procs, setProcs] = createStore<Proc[]>([]);
export { procs };
/**
 * Every message between a session and a workspace, as the bench publishes it. The bench is the
 * record (`/exchanges`); this is the live view of the same rows, and it is what shows a person
 * what became of an ask — it was reaching the renderer and being dropped on the floor.
 */
export type Exchange = { ts: number; id: string; session: string; workspace: string; dir: "in" | "out"; text: string; state: string; ref?: string };
const [exchanges, setExchanges] = createStore<Exchange[]>([]);
export { exchanges };
function foldExchange(row: Exchange) {
  const i = exchanges.findIndex((e) => e.id === row.id);
  // A transition carries only id and state; the row it updates keeps everything else.
  if (i >= 0) setExchanges(i, (e) => ({ ...e, ...row }));
  else setExchanges(produce((es) => void es.push(row)));
}
/** The bench's cached rows at connect, so a fresh window is not blank until the next change. */
export function seedExchanges(rows: unknown[]) {
  for (const r of rows as Exchange[]) if (r?.id) foldExchange(r);
}
/** Everything said to and from one workspace. */
export const exchangesOf = (workspace: string) => exchanges.filter((e) => e.workspace === workspace);
/** What a session has asked of a workspace and has not had back yet: its own queue. */
export const asksOf = (session: string) => exchanges.filter((e) => e.session === session && e.dir === "out" && e.state !== "done" && e.state !== "failed");

/**
 * Workspace id → the name a person gave it. An intercept, an exchange and a process all name a
 * workspace by id, and an id is unreadable and (truncated in a narrow row) indistinguishable from
 * another — "ws-30b60ec8…" told the owner nothing about which workspace took his traffic.
 */
const [wsNames, setWsNames] = createSignal<Record<string, string>>({});
export function setWorkspaceNames(rows: { id: string; name?: string }[]) {
  setWsNames(Object.fromEntries(rows.map((w) => [w.id, w.name || w.id])));
}
export const wsName = (id: string) => wsNames()[id] ?? id;

const [sessionCount, setSessionCount] = createSignal(1);
export { sessionCount, setSessionCount };
/** Sessions whose workspace messages were discarded with them; queues hide these. */
const [discarded, setDiscarded] = createSignal(new Set<string>());
export { discarded };
export function discard(id: string) {
  setDiscarded((d) => new Set(d).add(id));
}

/** Whether /events is up; false until main says otherwise. Offline, every thread reads and nothing sends. */
const [connected, setConnected] = createSignal(false);
const [writable, setWritable] = createSignal<{ ok: boolean; reason?: string }>({ ok: true });
export { connected, setConnected, writable };

/**
 * A person's answer to a proposal. The bench is holding the tool call until this lands; the answer
 * is also said out loud in the thread, so the transcript reads as the conversation it was.
 */
export function answerProposal(session: string, id: string, answer: "yes" | "no") {
  thread(session).proposal({ id, tool: "", summary: "", answer });
  thread(session).sent(answer);
  void window.harness.bench("POST", `/proposals/${id}`, { answer }).catch((e: Error) => thread(session).note(e.message));
}

export function stopProc(p: Proc) {
  if (p.session) void window.harness.pi({ type: "prompt", message: `/proc-stop ${p.id}` }, p.session).catch((e: Error) => thread(p.session!).note(e.message));
}

export function cancel(t: Task) {
  const i = taskIndex(t.id);
  if (i >= 0) setTasks(i, { state: "cancelled" });
  void window.harness.pi({ type: "prompt", message: `/cancel ${t.n ? `#${t.n}` : t.id}` }, t.session).catch((e: Error) => thread(t.session).note(e.message));
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

  /**
   * A tool asking to run. It sits in the transcript as a question and stays there once answered —
   * the record of what was agreed to is the conversation itself.
   */
  function proposal(row: { id: string; tool: string; summary: string; args?: Record<string, unknown>; answer?: "yes" | "no" }) {
    const i = messages.findIndex((m) => m.role === "question" && (m as { id: string }).id === row.id);
    if (i >= 0) return void setMessages(i, { answer: row.answer } as never);
    push({ role: "question", id: row.id, tool: row.tool, summary: row.summary, args: row.args, at: now() });
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
    // The echo the person sees as they hit enter. pi reports taking it a moment later, and THAT
    // is the message with the real time; this row is stamped from it rather than duplicated.
    push({ role: "user", text, at: now(), images, local: true });
  }

  /** The text of a message pi reports, whichever shape it is in. */
  const textOf = (m: { content?: unknown }) =>
    typeof m.content === "string" ? m.content : ((m.content as { text?: string }[] | undefined) ?? []).map((c) => c.text ?? "").join("");

  /** The user message pi's last message_start already placed, so message_end does not place it twice. */
  let placedUser = "";

  /**
   * The person's prompt, AT THE POINT PI TOOK IT. A queued line used to be echoed on
   * `queue_update`, which pi sends after the turn it started has already streamed its first
   * words — so the answer appeared above the question, dated when it was delivered rather than
   * when it was typed. Here the stream position is the truth and `message.timestamp` is the time.
   */
  function userMessage(m: { content?: unknown; timestamp?: unknown } | undefined) {
    const text = textOf(m ?? {});
    if (!text.trim()) return;
    const key = `${String(m?.timestamp ?? "")}\u0000${text}`;
    if (key === placedUser) return;
    placedUser = key;
    const ts = typeof m?.timestamp === "number" ? m.timestamp : Date.now();
    const at = new Date(ts).toTimeString().slice(0, 5);
    // The local echo of this very prompt: stamp it instead of showing the line twice.
    let i = -1;
    for (let k = messages.length - 1; k >= 0 && i < 0; k--) if (messages[k].role === "user" && (messages[k] as { local?: true; text?: string }).local && (messages[k] as { text?: string }).text === text) i = k;
    if (i >= 0) return setMessages(i, { at, ts, local: undefined } as never);
    push({ role: "user", text, at, ts });
    // A user message ends whatever assistant block was open: the next delta starts a new one.
    open = -1;
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
      case "queue_update": {
        // pi is the truth about what it still HOLDS, and nothing more: a line it has taken is
        // echoed by `message_start`, at the point in the stream where it was taken. Echoing it
        // here put the prompt after the answer to it, because pi reports the queue late.
        const held = new Set([...((ev.steering as string[]) ?? []), ...((ev.followUp as string[]) ?? [])]);
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
          const keep = ts.filter((t) => t.state === "running" || t.state === "background" || t.state === "lost" || (t.ended && now - t.ended < 4000));
          ts.splice(0, ts.length, ...keep);
        }));
        return;
      case "message_start": {
        const m = ev.message as { role?: string; content?: unknown; timestamp?: unknown } | undefined;
        if (m?.role === "user") userMessage(m);
        return;
      }
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
        const m = ev.message as { role?: string; customType?: string; content?: any; timestamp?: unknown } | undefined;
        // message_end is authoritative (rpc.md): a start that carried no content yet lands here.
        if (m?.role === "user") userMessage(m);
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

  return { id, messages, busy, status, setStatus, ready, attachments, attach, detach, takeAttachments, replay, note, sent, queued, queue, proposal, onEvent };
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
/** Session events carry the session they came from; the bench's own changes carry none. */
export function onEvent(ev: Ev & { pi?: string }) {
  switch (ev.type) {
    case "bench":
      return void setConnected(ev.connected === true);
    case "writable":
      return void setWritable({ ok: ev.ok === true, reason: ev.reason as string | undefined });
    case "proposal": {
      const row = ev.row as { id: string; session: string; tool: string; summary: string; args?: Record<string, unknown>; answer?: "yes" | "no" };
      if (row?.session) thread(row.session).proposal(row);
      return;
    }
    case "exchange":
      // One row per publish: a record, or a transition of one already held.
      if (ev.row) foldExchange(ev.row as Exchange);
      return;
    case "procs":
      // The bench folds every session's widget into one table; it is the whole list.
      return void setProcs(produce((ps) => void ps.splice(0, ps.length, ...((ev.rows as Proc[]) ?? []))));
    case "task": {
      // The bench's ledger is the record; the live fold only adds output.
      const row = ev.row as Task;
      const i = taskIndex(row.id);
      if (i >= 0) setTasks(i, { ...row, output: tasks[i].output });
      else if (row.state === "running" || row.state === "background" || row.state === "lost") setTasks(produce((ts) => void ts.push({ ...row, output: row.output ?? "" })));
      return;
    }
  }
  if (typeof ev.pi === "string" && ev.pi) thread(ev.pi).onEvent(ev);
}

// The bench, under the names everything already reads.
const bench = thread("bench");
export const { messages, busy, status, ready, attachments, attach, detach, takeAttachments, replay, note, sent } = bench;
