import { createSignal, untrack } from "solid-js";
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
/**
 * Everything a session has sent to a workspace, newest last — its Queue. Read from the exchange
 * store, which is keyed by session and folded on every `exchange` event; the panel used to read
 * `workspace.queue`, which `toWorkspace()` always fills with `[]`, so it was empty for a session
 * the bench had rows for (owner: s-7 with an `info-*` row on ws-632c…).
 *
 * `info-*` rows and done ones are included: the Queue is what was asked, not only what is pending.
 */
/**
 * What this session is WAITING ON, for the footer: "waiting on backend · 2m". An open handoff that
 * only exists in a panel somebody has to open is a person kept in the dark (owner, 2026-09-18), and
 * the composer is where they are already looking.
 */
export const waitingFor = (session: string, now = Date.now()): string | undefined => {
  const open = exchanges
    .filter((e) => e.session === session && e.dir === "out" && (e.state === "queued" || e.state === "running"))
    .sort((a, b) => a.ts - b.ts);
  if (!open.length) return undefined;
  const mins = Math.max(0, Math.round((now - open[0].ts) / 60_000));
  const age = mins < 1 ? "just now" : `${mins}m`;
  const name = wsNames()[open[0].workspace] ?? open[0].workspace;
  return open.length === 1 ? `waiting on ${name} · ${age}` : `waiting on ${open.length} · oldest ${name} · ${age}`;
};

export const queueOf = (session: string) =>
  exchanges
    .filter((e) => e.session === session && e.dir === "out")
    .slice()
    .sort((a, b) => a.ts - b.ts)
    .map((e) => ({
      dir: e.dir,
      text: e.text,
      session: e.session,
      // The store keeps a timestamp; the row renders a clock.
      at: new Date(e.ts).toTimeString().slice(0, 5),
      state: (e.state === "done" || e.state === "failed" ? "done" : e.state === "working" ? "working" : "pending") as "pending" | "working" | "done",
      workspace: wsNames()[e.workspace] ?? e.workspace,
    }));

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

/**
 * What each session said it would do. The PLAN panel used to draw a hardcoded empty list — pi has
 * no todo events of its own, so the plan comes from the model's own `kl_plan` through the bench.
 */
export type PlanRow = { text: string; state: "todo" | "doing" | "done" | "later"; why?: string };
const [plans, setPlans] = createStore<Record<string, PlanRow[]>>({});
export { plans };
/** The panel's own four states, which the tree already draws: a plan is a plan either way. */
const PLAN_STATE = { todo: "pending", doing: "active", done: "done", later: "blocked" } as const;
export const planOf = (session: string) =>
  // The harness tags an item it made for an ask with its exchange id; the person reads the words.
  (plans[session] ?? []).map((x, i) => ({ id: `${session}-${i}`, text: x.text.split("\u0000")[0], state: PLAN_STATE[x.state] ?? "pending", note: x.why }));

/**
 * Build or Plan, and how hard the model thinks — opencode's `tab` and `ctrl+t`, on our own shapes.
 * PLAN is read-only tools plus `plan`: a person who wants a plan before anything changes should be
 * able to have one without trusting the model not to change anything.
 */
/**
 * Three modes, cycled with shift+tab as Claude Code does (§21). ACCEPT-EDITS answers the
 * proposals for this machine's own files by itself — write and edit, nothing else. A platform
 * write is never auto-answered: somebody else's workspace, an environment, a deletion is a
 * decision, not an edit.
 */
export type Mode = "build" | "plan" | "accept-edits";
export const MODES: Mode[] = ["build", "plan", "accept-edits"];
const [mode, setMode] = createSignal<Mode>("build");
export { mode, setMode };
/** What accept-edits may answer for the person. Everything else still asks. */
// accept-edits answers for the FILE tools only. A command is not an edit: it can reach the network,
// delete a tree or start a server, and "accept edits" never meant "accept anything" (owner,
// 2026-09-18).
export const AUTO_YES = ["write", "edit", "patch"];

/**
 * Tools this person has said "don't ask again" about, for this session only. It is the permission
 * prompt's second option, and it is deliberately not remembered past the session: a standing yes
 * that outlives the work it was given for is how something gets agreed to twice.
 */
const [allowed, setAllowed] = createSignal<Record<string, string[]>>({});
export const allowsTool = (session: string, tool: string) => (allowed()[session] ?? []).includes(tool);

/**
 * How many questions a thread is waiting on. A proposal belongs to the session that raised it —
 * `s-2`, not whichever session a window happens to be looking at — so a tab that is not open, or
 * not in front, has to SAY it is holding one (owner, 2026-09-17: the desktop showed no card at all
 * while the bench held an open proposal for another session).
 */
export const waitingOn = (session: string) =>
  thread(session).messages.filter((m) => m.role === "question" && !(m as { answer?: string }).answer).length;
export function allowTool(session: string, tool: string) {
  setAllowed((a) => ({ ...a, [session]: [...new Set([...(a[session] ?? []), tool])] }));
}
export const LEVELS = ["low", "medium", "high"] as const;
const [level, setLevel] = createSignal<(typeof LEVELS)[number]>("low");
export { level, setLevel };
// Only shown once somebody has set it: a default we invented is not a fact about the model.
const [levelKnown, setLevelKnown] = createSignal(false);
export { levelKnown };
export const noteLevel = (l: (typeof LEVELS)[number]) => (setLevel(l), setLevelKnown(true));

/** Spec §1.2: the levels pi takes, and the effort levels a model that has one takes. */
export const THINKING = ["off", "minimal", "low", "medium", "high", "xhigh"] as const;
export const EFFORT = ["low", "medium", "high", "max"] as const;
export type ModelRow = { id: string; name: string; thinking: boolean; effort: boolean };
export type ProviderRow = { id: string; label: string; wired: boolean; models: ModelRow[] };

/**
 * `GET /models` once per window: pi's own snapshot of what the configured credentials reach, plus
 * every provider it supports. Cached because the dialog re-opens far more often than the answer
 * changes, and an empty answer is not cached — a bench with no child up yet lists no models, and
 * caching that would leave the picker empty for the life of the window.
 */
const [providers, setProviders] = createSignal<ProviderRow[]>([]);
export { providers };
export async function models(): Promise<ProviderRow[]> {
  if (providers().some((p) => p.models.length)) return providers();
  const r = (await window.harness.bench("GET", "/models").catch(() => undefined)) as { providers?: ProviderRow[] } | undefined;
  const rows = r?.providers ?? [];
  if (rows.length) setProviders(rows);
  return rows;
}

/**
 * Each session's current thinking and effort, kept up to date by App as the bench's rows arrive.
 * A turn stamps these onto the message it produced, so the footer of an OLD turn keeps saying what
 * actually answered it rather than following the live pick (owner, on the fleet).
 */
const sessionTriples = new Map<string, { model?: string; thinking?: string; effort?: string }>();

/**
 * The bench's row for a session, as it arrives. A turn stamps this onto the message it produced,
 * and a CHANGE draws one divider — the quiet line the transcript already uses for `Interrupted` and
 * `Session compacted`. It is renderer state derived from a bench event: nothing here is written to
 * pi's session file or shown to the model (owner: "don't spoil the session with this data").
 * The first sighting of a session is not a change, so opening a window draws nothing.
 *
 * `name` is the model's readable name, passed IN: this module is loaded by node:test, where an
 * extensionless `./rows` import does not resolve (Vite resolves it, Node does not).
 */
export function noteTriple(id: string, t: { model?: string; thinking?: string; effort?: string }, name = t.model): void {
  const had = sessionTriples.get(id);
  sessionTriples.set(id, t);
  if (!had) return;
  if (t.model && t.model !== had.model) thread(id).divider(`Model changed to ${name}`);
  if (t.thinking && t.thinking !== had.thinking) thread(id).divider(`Thinking ${t.thinking}`);
  if (t.effort && t.effort !== had.effort) thread(id).divider(`Effort ${t.effort}`);
}

/** Which dialog takes the composer's place, if any. One at a time, like the permission prompt. */
/**
 * TAB level: which dialog takes the composer's place, per tab. A window-wide signal opened the
 * picker in every tab at once and closing one closed them all — a dialog is a view's state, not
 * the window's. Keyed by tab id; `closeTab` drops it with the tab.
 */
const [dialogs, setDialogs] = createStore<Record<string, "model" | undefined>>({});
export const dialog = (tab: string) => dialogs[tab];
export const setDialog = (tab: string, d: "model" | undefined) => setDialogs(tab, d);
/** Everything TAB level holds for one tab, dropped together when it closes. */
export const closeTab = (tab: string) => setDialogs(tab, undefined);

/**
 * A person's pick, for one session. The bench writes the session's fields AND the general default
 * (spec §1.2) and re-applies the triple to the live child, so nothing here has to talk to pi.
 */
export async function setModel(session: string, patch: { model?: string; thinking?: string; effort?: string }): Promise<void> {
  await window.harness.bench("POST", `/sessions/${session}/model`, patch).catch((e: Error) => setStatusNote(e.message));
}

const [sessionCount, setSessionCount] = createSignal(1);
export { sessionCount, setSessionCount };
/** Sessions whose workspace messages were discarded with them; queues hide these. */
const [discarded, setDiscarded] = createSignal(new Set<string>());
export { discarded };
export function discard(id: string) {
  setDiscarded((d) => new Set(d).add(id));
}

/**
 * The bench's DEFAULT model (`GET /bootstrap`'s `model`, the fleet's `KL_MODEL`). A window that
 * opens before any session row has loaded still has to name what will answer.
 */
const [benchModel, setBenchModel] = createSignal<string | undefined>();
export { benchModel, setBenchModel };

/**
 * A transient failure, shown in the composer footer beside the retry status and gone on its own.
 * These used to be `note()` rows in the transcript — a failed bench call is a fact about the
 * DESKTOP, not part of the conversation, and it was being written into the session's history.
 */
const [statusNote, setStatusNote_] = createSignal<string | undefined>();
export { statusNote };
let statusTimer: ReturnType<typeof setTimeout> | undefined;
export function setStatusNote(text: string | undefined, ms = 6000) {
  clearTimeout(statusTimer);
  setStatusNote_(text);
  if (text) statusTimer = setTimeout(() => setStatusNote_(undefined), ms);
}

/**
 * How long a drop is allowed to last before the desktop calls it an outage. The `/events` socket is
 * reconnected in under a second, and painting "bench offline" for that flicker made a healthy
 * desktop look broken ~25 times an hour. A real outage still shows, 3 s late.
 */
export const OFFLINE_AFTER_MS = 3000;
let offlineTimer: ReturnType<typeof setTimeout> | undefined;

/**
 * `true` takes effect at once and cancels a pending "offline"; `false` waits, so a reconnect inside
 * the window is never seen. Exported for the test: the timing is the whole behaviour.
 */
export function noteConnected(up: boolean, apply: (v: boolean) => void = setConnected, ms = OFFLINE_AFTER_MS): void {
  clearTimeout(offlineTimer);
  offlineTimer = undefined;
  if (up) return void apply(true);
  offlineTimer = setTimeout(() => (offlineTimer = undefined, apply(false)), ms);
}

/** Whether /events is up; false until main says otherwise. Offline, every thread reads and nothing sends. */
const [connected, setConnected] = createSignal(false);
const [writable, setWritable] = createSignal<{ ok: boolean; reason?: string }>({ ok: true });
export { connected, setConnected, writable };

/**
 * A person's answer to a proposal. The CARD is the record — it already reads "You answered: … →
 * yes" — so nothing else is pushed: a `> yes` user row under it said the same thing twice (owner,
 * on the transcript). Nothing goes to pi either: the bench is holding the tool call and wakes it
 * with the answer, which reaches the model as that tool's RESULT, never as something the person
 * typed.
 */
export function answerProposal(session: string, id: string, answer: string) {
  thread(session).proposal({ id, tool: "", summary: "", answer });
  void window.harness.bench("POST", `/proposals/${id}`, { answer }).catch((e: Error) => setStatusNote(e.message));
}

/**
 * A person stopping a process. Plain HTTP to the bench: as a `/proc-stop` PROMPT it sat in pi's
 * context and in its session file, so every reopen replayed it as something the person had said
 * (owner: "don't spoil the session with this data").
 */
export function stopProc(p: Proc) {
  if (p.session) void window.harness.bench("POST", `/procs/${p.id}/stop`, { session: p.session }).catch((e: Error) => setStatusNote(e.message));
}

/** Likewise a person cancelling a task: recorded by the bench, never spoken to the model. */
export function cancel(t: Task) {
  const i = taskIndex(t.id);
  if (i >= 0) setTasks(i, { state: "cancelled" });
  void window.harness.bench("POST", `/tasks/${t.id}/cancel`, {}).catch((e: Error) => setStatusNote(e.message));
}

/**
 * A slash line is a COMMAND, not something the person said. None of them is ever a transcript row —
 * not the ones the renderer handles (`/model`, `/clear`, …) and not the ones the harness sends to
 * pi on the person's behalf (`/proc-stop`, `/cancel`), which pi also writes into the session file,
 * so the replay would render them again on every reopen (owner, on the fleet: `> /model 23:56`).
 * Applied at every door a user row can come through: the local echo, pi's own report, and replay.
 */
export const isCommandLine = (text: string): boolean => /^\s*\//.test(text);

/**
 * A bare answer that pi recorded right after a proposal or question tool call. The card IS the
 * record — "You answered: … → yes" — and the `> yes` row under it said the same thing twice
 * (owner, on the transcript). Live, nothing pushes it any more; an OLD session file still holds
 * one, so replay drops it. Only a bare yes/no/approve-style word immediately after such a call:
 * anything a person actually wrote is their message and stays.
 */
const ANSWER_WORDS = /^(y|n|yes|no|ok|okay|sure|approve|approved|allow|allowed|deny|denied|reject|rejected)[.!]?$/i;
export const isCardAnswer = (text: string, afterCard: boolean): boolean => afterCard && ANSWER_WORDS.test(text.trim());

/**
 * Tool calls that waited on a PERSON. Their row is the card, never a `Called …` line: the live path
 * has skipped them since b1119b9a, and replay does the same, so an old session file reads as it was
 * lived rather than as a tool that took five minutes.
 */
const WAITS_ON_PERSON = new Set(["question", "ask_close"]);

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
    /** Whether the last thing pi recorded was a card the person answers — a question or a proposal. */
    let afterCard = false;
    /** Calls that waited on a person, by id: they become their card when the answer arrives. */
    const waited = new Map<string, { id: string; args: Record<string, unknown>; at: string; ts?: number }>();
    for (const m of raw as Record<string, unknown>[]) {
      const ts = typeof m.timestamp === "number" ? m.timestamp : undefined;
      const at = ts ? new Date(ts).toTimeString().slice(0, 5) : "";
      if (m.role === "user") {
        const c = m.content;
        const text = typeof c === "string" ? c : (c as { type: string; text?: string }[]).map((x) => x.text ?? "").join("");
        // A card's own answer is already drawn by the card; a command was never a message at all.
        if (text.trim() && !isCommandLine(text) && !isCardAnswer(text, afterCard)) out.push({ role: "user", text, at, ts });
        afterCard = false;
      } else if (m.role === "assistant") {
        for (const c of m.content as Record<string, unknown>[]) {
          if (c.type === "text" && (c.text as string).trim()) out.push({ role: "assistant", text: c.text as string, at, ts });
          if (c.type === "toolCall") {
            const name = c.name as string;
            const args = c.arguments as Record<string, unknown>;
            // A call that waited on a PERSON is its card, never a tool row — the same rule the live
            // path follows. Replayed, it drew as ``● Called `question` … 305.4s``, which is the
            // question said a second time and a clock nobody was waiting on (owner, 2026-09-18).
            if (WAITS_ON_PERSON.has(name)) {
              waited.set(c.id as string, { id: c.id as string, args, at, ts });
              afterCard = true;
              continue;
            }
            out.push({ role: "action", kind: name === "bash" ? "run" : "note", target: TOOL[name] ?? name, text: argOf(name, args), at, ts, output: "", tool: name, args });
            calls.set(c.id as string, out.length - 1);
            if (name === "ask") afterCard = true;
          }
        }
      } else if (m.role === "toolResult") {
        const asked = waited.get(m.toolCallId as string);
        if (asked) {
          // What the person answered IS the record: the card, already settled.
          const content = (m.content as { type?: string; text?: string }[]) ?? [];
          const said = content.map((x) => x.text ?? "").join("").trim();
          const q = asked.args as { header?: string; question?: string; options?: unknown };
          waited.delete(m.toolCallId as string);
          if (!(m.isError as boolean) && said)
            out.push({
              role: "question",
              id: asked.id,
              tool: "question",
              summary: String(q.question ?? q.header ?? ""),
              args: asked.args,
              ask: { header: String(q.header ?? "Question"), options: (q.options as { label: string; description: string }[] | undefined) ?? [] },
              answer: said,
              at: asked.at,
              ts: asked.ts,
            });
          continue;
        }
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
  function proposal(row: { id: string; tool: string; summary: string; preview?: string; args?: Record<string, unknown>; answer?: string; question?: unknown }) {
    const i = messages.findIndex((m) => m.role === "question" && (m as { id: string }).id === row.id);
    if (i >= 0) return void setMessages(i, { answer: row.answer } as never);
    push({ role: "question", id: row.id, tool: row.tool, summary: row.summary, preview: row.preview, args: row.args, ask: row.question as never, at: now() });
  }

  /** A line from the harness itself, on the rail, the way a shell answers a builtin. */
  /** A line across the transcript, not a message: what happened to the turn itself. */
  function divider(text: string) {
    push({ role: "divider", text, at: now() });
  }

  function note(text: string) {
    push({ role: "action", kind: "note", target: "harness", text: text.split("\n")[0], at: now(), ok: true, output: text.includes("\n") ? text : undefined });
  }

  // What is waiting on the bench: pi's own queue, as it reports it. A line the
  // person queued is shown here, not in the transcript, until pi delivers it.
  const [queue, setQueue] = createStore<{ text: string; how: "queue" | "steer"; reason?: string }[]>([]);
  function queued(text: string, how: "queue" | "steer") {
    setQueue(produce((q) => void q.push({ text, how })));
  }
  /** The order a fork of this session chose, and why — shown under each row so it is not a mystery. */
  function reorder(rows: { text: string; reason?: string }[]) {
    setQueue(produce((q) => {
      const rest = q.filter((x) => !rows.some((r) => r.text === x.text));
      q.splice(0, q.length, ...rows.map((r) => ({ text: r.text, how: "queue" as const, reason: r.reason })), ...rest);
    }));
  }

  function sent(text: string, images: number[] = []) {
    // The echo the person sees as they hit enter. pi reports taking it a moment later, and THAT
    // is the message with the real time; this row is stamped from it rather than duplicated.
    if (isCommandLine(text)) return;
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
    if (!text.trim() || isCommandLine(text)) return;
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

  /**
   * What this turn is doing, for the one status line under the transcript: a verb that changes as
   * it goes, when it started, and what it has spent. A person watching an agent wants to know it is
   * alive and on what — "Working…" for four minutes says neither.
   */
  const [turn, setTurn] = createSignal<{ verb: string; since: number; tokens: number } | undefined>();
  /** The retry in flight, if any: which attempt, and what failed the time before. */
  const [retry, setRetry] = createSignal<{ attempt: number; message: string } | undefined>();
  /** What this session has spent so far: pi reports it cumulatively, and the header shows it. */
  const [spend, setSpend] = createSignal<{ tokens: number; cost: number; context?: number }>({ tokens: 0, cost: 0 });
  const doing = (verb: string) => setTurn((t) => ({ verb, since: t?.since ?? Date.now(), tokens: t?.tokens ?? 0 }));

  /** The assistant message being streamed, by index into `messages`. */
  let open = -1;
  /** The reasoning block being streamed, the same way — thinking and answer stream side by side. */
  let reasoning = -1;
  const tools = new Map<string, number>();

  function onEvent(ev: Ev) {
    switch (ev.type) {
      case "started":
        setStatus(`${ev.model as string} · ${ev.host as string}${ev.resumed ? " · resumed" : ""}${ev.forked ? " · read-only fork" : ""}`);
        return;
      case "exit":
        // ONE line about an exit: the status bar carries it, and the transcript gets the note.
        // Both the exit and the stderr that preceded it used to write their own row (owner, 2026-09-17).
        setStatus(`pi exited (${String(ev.code)})${ev.stderr ? `: ${String(ev.stderr).slice(0, 300)}` : ""}`);
        if (ev.stderr) note(`pi exited (${String(ev.code)}): ${String(ev.stderr)}`);
        setBusy(false);
        return;
      case "stderr":
        // Only a failure to START is worth surfacing, and only in the status: the exit above says
        // it in the transcript, once.
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
        setTurn({ verb: "Thinking", since: Date.now(), tokens: 0 });
        return;
      case "agent_end":
        // A run that will be retried is not an answer: the footer says so, with the attempt, rather
        // than going quiet and then surprising the person with a second turn (`session-retry.tsx`).
        if (ev.willRetry === true) {
          setRetry((r) => ({ attempt: (r?.attempt ?? 0) + 1, message: String((ev.error as { message?: string } | undefined)?.message ?? "") }));
          return;
        }
        setRetry(undefined);
        // How long the turn actually took, stamped on the answer it produced: the footer said
        // "0s" because nothing ever wrote one (owner, 2026-09-17).
        {
          const since = untrack(turn)?.since;
          if (since !== undefined) {
            for (let k = messages.length - 1; k >= 0; k--) {
              const m = messages[k];
              if (m.role === "user") break;
              if (m.role === "assistant" && (m as { kind?: string }).kind !== "reasoning" && (m as { ms?: number }).ms === undefined) {
                setMessages(k, { ms: Date.now() - since } as never);
                break;
              }
            }
          }
        }
        setBusy(false);
        setTurn(undefined);
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
        // pi reports cumulative usage as it streams; a provider that reports none leaves it at 0.
        const u = ev.usage as { totalTokens?: number; cost?: { total?: number }; contextWindow?: number } | undefined;
        if (typeof u?.totalTokens === "number" && u.totalTokens) {
          setTurn((t) => (t ? { ...t, tokens: u.totalTokens! } : t));
          setSpend((p) => ({ tokens: u.totalTokens!, cost: u.cost?.total ?? p.cost, context: u.contextWindow ?? p.context }));
        }
        const d = ev.assistantMessageEvent as { type: string; delta?: string } | undefined;
        if (d?.type === "text_delta" && d.delta) doing("Writing");
        // Thinking is its own block, kept apart from the answer: it is the model working, and it
        // must never be read as what it decided.
        if (d?.type === "thinking_delta" && d.delta) {
          doing("Thinking");
          if (reasoning < 0) {
            push({ role: "assistant", text: d.delta, at: now(), kind: "reasoning" });
            reasoning = messages.length - 1;
            open = -1;
          } else {
            setMessages(reasoning, "text" as never, ((t: string) => t + d.delta!) as never);
          }
          return;
        }
        if (d?.type !== "text_delta" || !d.delta) return;
        reasoning = -1;
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
        reasoning = -1;
        // What ANSWERED this turn, stamped on the message it produced (never read from the live
        // line): pi's own message names the model and provider, and the session row names the
        // thinking and effort that were in force at this instant.
        {
          const a = ev.message as { role?: string; model?: unknown; provider?: unknown } | undefined;
          if (a?.role === "assistant" && typeof a.model === "string") {
            const answered = typeof a.provider === "string" && a.provider ? `${a.provider}/${a.model}` : a.model;
            const t = sessionTriples.get(id) ?? {};
            for (let k = messages.length - 1; k >= 0; k--) {
              const m = messages[k];
              if (m.role === "user") break;
              if (m.role === "assistant" && (m as { kind?: string }).kind !== "reasoning" && (m as { model?: string }).model === undefined) {
                setMessages(k, { model: answered, thinking: t.thinking, effort: t.effort } as never);
                break;
              }
            }
          }
        }
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
        // A turn blocked in a tool — a proposal waiting on a person, a long command — is still a
        // turn: nothing streams, and the desktop read that as idle, sent a plain prompt, and
        // printed pi's refusal (owner, 2026-09-17).
        setBusy(true);
        const args = ev.args as Record<string, unknown>;
        const name = ev.toolName as string;
        // The verb names what is actually happening: "Running bash…", "Waiting on agent svelte…".
        doing(name === "ask" ? `Waiting on ${args.to === "agent" ? `agent ${args.name ?? ""}`.trim() : args.to}` : name === "question" ? "Asking" : `Running ${TOOL[name] ?? name}`);
        // A QUESTION is its own card and nothing else: the proposal row carries the header, the
        // options and the answer. A tool row beside it printed the same question a second time,
        // under a generic `Called \`question\`` line with its arguments spelled out
        // (owner's screenshot, 2026-09-17). The card is the only rendering.
        if (name === "question") return;
        push({ role: "action", kind: name === "bash" ? "run" : "note", target: TOOL[name] ?? name, text: argOf(name, args), at: now(), pending: true, tool: name, args });
        tools.set(ev.toolCallId as string, messages.length - 1);
        // Only work that runs on its own is a task; a call waiting on the person is the card in the
        // composer, not a row in BACKGROUND TASKS (owner, 2026-09-17).
        if (bench && name !== "question" && name !== "ask_close")
          setTasks(produce((ts) => void ts.push({ id: ev.toolCallId as string, session: id, tool: TOOL[name] ?? name, arg: argOf(name, args), state: "running", started: Date.now(), output: "" })));
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

  return { id, messages, busy, turn, retry, spend, reorder, status, setStatus, ready, attachments, attach, detach, takeAttachments, replay, note, divider, sent, queued, queue, proposal, onEvent };
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
      return void noteConnected(ev.connected === true);
    // Main saying something transient — a bench route that refused and is being re-minted. It is a
    // fact about the DESKTOP, so it goes to the composer footer and never into the transcript.
    // A turn the model PROVIDER refused. Said in the transcript as a divider — it is a fact about
    // this turn, worth reading back — and in the footer, because it is why nothing came back.
    case "turn_error": {
      const at = typeof ev.session === "string" ? ev.session : undefined;
      if (!at) return;
      thread(at).divider(`The model refused this turn: ${String(ev.text ?? "").slice(0, 160)}`);
      return void setStatusNote(`the model refused this turn: ${String(ev.text ?? "").slice(0, 120)}`);
    }
    case "status":
      return void setStatusNote(typeof ev.text === "string" ? ev.text : undefined);
    case "writable":
      return void setWritable({ ok: ev.ok === true, reason: ev.reason as string | undefined });
    case "proposal": {
      const row = ev.row as { id: string; session: string; tool: string; summary: string; preview?: string; args?: Record<string, unknown>; answer?: string; question?: unknown };
      if (!row?.session) return;
      thread(row.session).proposal(row);
      // In accept-edits, a change to this machine's own files is answered without asking.
      // Already agreed to for this session, or accept-edits on this machine's own files: answered
      // without asking again.
      if (!row.answer && (allowsTool(row.session, row.tool) || (mode() === "accept-edits" && AUTO_YES.includes(row.tool))))
        answerProposal(row.session, row.id, "yes");
      return;
    }
    case "queue_order":
      if (typeof ev.session === "string") thread(ev.session).reorder((ev.items as { text: string; reason?: string }[]) ?? []);
      return;
    case "compacted":
      // The conversation was summarised and carried on: one row, so a person is not left
      // wondering where the middle of their transcript went.
      if (typeof ev.session === "string") thread(ev.session).divider("Session compacted");
      return;
    case "plan": {
      if (typeof ev.session !== "string") return;
      const before = plans[ev.session];
      setPlans(ev.session, (ev.items as PlanRow[]) ?? []);
      // The PLAN panel is the surface for this: a `Todo: …` row repeated the panel into the
      // transcript on every plan change and pushed the conversation down.
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

/**
 * Two presses of escape stop the turn (`prompt/index.tsx:408`): one is too easy to hit by accident
 * while reading, and a turn that dies because a person tapped a key is worse than one that runs a
 * moment longer. The abort is pi's own, and the thread says so where the turn ended.
 */
export function interrupt(session: string) {
  thread(session).divider("Interrupted");
  void window.harness.pi({ type: "abort" }, session).catch((e: Error) => setStatusNote(e.message));
}

/** A queued line, sent now rather than in its turn: pi's `steer` puts it into the running turn. */
export function sendNow(session: string, text: string) {
  void window.harness.pi({ type: "steer", message: text }, session).catch((e: Error) => setStatusNote(e.message));
}

/** `12.4K (38%) · $1.20` — what the turn has spent, the way opencode's footer says it (`:268`). */
const money = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });
export function usage(tokens: number, cost?: number, context?: number): string {
  const n = tokens > 1000 ? `${Math.round(tokens / 100) / 10}K` : String(tokens);
  const pct = context ? ` (${Math.min(100, Math.round((tokens / context) * 100))}%)` : "";
  return [`${n}${pct}`, cost ? money.format(cost) : ""].filter(Boolean).join(" · ");
}

/**
 * A workspace's own files, read from its tool server through the bench (`/fs/tree`, `/fs/changes`).
 * The desktop drew an empty FILES section and "Nothing differs from ." because nobody ever asked
 * for them — `toWorkspace()` filled both with `[]` (owner, 2026-09-17).
 *
 * Cached per workspace and per directory, because a tree is asked for again every time a fold
 * opens, and the answer carries an ETag the tool server would rather we reused.
 */
/**
 * One row of `/fs/tree`, as the tool server writes it (`crates/ide/src/fs/tree.rs:14`): `kind` is
 * `dir` / `file` / `symlink`, `ignored` is what the global gitignore and `.git` cover, and `git` is
 * one letter of status. We read ITS field names — a `dir` boolean of our own invention is how every
 * entry came out looking like a file (owner, 2026-09-18).
 */
export type FsEntry = { name: string; kind?: "dir" | "file" | "symlink"; ignored?: boolean; git?: string; size?: number; target?: string };
/**
 * `/fs/changes`, in the tool server's own words (`crates/ide/src/fs/git.rs:12,19`): two porcelain
 * columns per change, plus where the head is. It sends no `status`, no `add` and no `del` — reading
 * those is why the CHANGES tab was empty for a workspace with real changes (owner, 2026-09-18).
 */
export type FsChanges = {
  repo: boolean;
  branch?: string;
  head?: string;
  ahead?: number;
  behind?: number;
  changes: { path: string; index?: string; worktree?: string; renamed_from?: string }[];
};

/**
 * What has been read, per workspace, and a counter each view reads so a patched cache redraws.
 *
 * The cache SURVIVES tab and session switches: opening the Files tab draws what was read before and
 * asks for nothing (owner: "it's taking time to load everything every time"). What keeps it honest
 * is the watch stream below, not a refetch.
 */
const fsCache = new Map<string, unknown>();
const [fsVersion, setFsVersion] = createSignal(0);
/** Read this in a view that draws files: it changes when the watch patched what is cached. */
export const fsChanged = fsVersion;
const bump = () => setFsVersion((n) => n + 1);

/** The key a directory listing is cached under; `""` is the workspace root. */
const treeKey = (scope: string, path?: string) => `tree?${new URLSearchParams({ scope, ...(path ? { path } : {}) }).toString()}`;
const parentOf = (path: string) => (path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "");

/**
 * One watch event, applied to what is already held. A created or deleted path is a row inserted or
 * removed under ITS parent — never a reason to re-read the tree, which is what made every visit
 * cost a full walk. A modified file only loses its text, so the next open re-reads that one file.
 *
 * `kind` is notify's own word, lowercased by the tool server: `create`, `modify`, `remove`, `any`.
 */
export function applyWatch(scope: string, ev: { path?: string; kind?: string; resync?: true }): "patched" | "resync" | "ignored" {
  if (ev.resync || !ev.path) return "resync";
  const path = ev.path;
  const name = path.slice(path.lastIndexOf("/") + 1);
  tags.delete(`${scope}:${path}`);
  fsCache.delete(`diff?${new URLSearchParams({ scope, path }).toString()}`);
  const key = treeKey(scope, parentOf(path) || undefined);
  const held = fsCache.get(key) as { entries?: FsEntry[] } | undefined;
  if (held) {
    const entries = held.entries ?? [];
    const at = entries.findIndex((e) => e.name === name);
    if (ev.kind === "remove") {
      if (at >= 0) fsCache.set(key, { ...held, entries: entries.filter((_, i) => i !== at) });
    } else if (at < 0) {
      // A path we have never seen: the kind alone cannot say whether it is a directory, so ask the
      // one thing that knows. Until it answers it is a file, which is what it usually is.
      fsCache.set(key, { ...held, entries: [...entries, { name, kind: "file" }] });
      void fsStat(scope, path).then((st) => {
        if (st?.kind !== "dir") return;
        const now = fsCache.get(key) as { entries?: FsEntry[] } | undefined;
        if (!now) return;
        fsCache.set(key, { ...now, entries: (now.entries ?? []).map((e) => (e.name === name ? { ...e, kind: "dir" as const } : e)) });
        bump();
      });
    }
    // A directory that is gone takes what was read under it; folds are the panel's, so they stay.
    if (ev.kind === "remove") for (const k of [...fsCache.keys()]) if (k.startsWith(treeKey(scope, path))) fsCache.delete(k);
  }
  // Git's own view of the workspace changed too, but only once per burst: a save writes many
  // events and `/fs/changes` walks the whole status.
  dirtyChanges(scope);
  bump();
  return "patched";
}

/** `/fs/changes` re-read once a burst has settled, never per event. */
const CHANGES_SETTLE_MS = 500;
const settling = new Map<string, ReturnType<typeof setTimeout>>();
function dirtyChanges(scope: string) {
  clearTimeout(settling.get(scope));
  settling.set(
    scope,
    setTimeout(() => {
      settling.delete(scope);
      fsCache.delete(`changes?${new URLSearchParams({ scope }).toString()}`);
      void fsChanges(scope).then(bump);
    }, CHANGES_SETTLE_MS),
  );
}

/** Everything this workspace held, dropped: what the stream missed cannot be patched in. */
export function refetchFs(scope: string): void {
  for (const k of [...fsCache.keys()]) if (k.includes(`scope=${encodeURIComponent(scope)}`)) fsCache.delete(k);
  for (const k of [...tags.keys()]) if (k.startsWith(`${scope}:`)) tags.delete(k);
  bump();
}

/**
 * One watch per workspace on show. The stream itself — dialling, the backoff, the redial — is
 * main's; here it is only what an event means to what is cached.
 */
const watching = new Set<string>();
export function watchFs(scope: string): void {
  if (watching.has(scope)) return;
  watching.add(scope);
  void window.harness.watch.open(scope).catch(() => watching.delete(scope));
}
export function unwatchFs(scope: string): void {
  if (!watching.delete(scope)) return;
  window.harness.watch.close(scope);
}
if (typeof window !== "undefined" && window.harness?.watch)
  window.harness.watch.onEvent((scope, ev) => {
    if (applyWatch(scope, ev) === "resync") refetchFs(scope);
  });

/**
 * Which TREE of the workspace a read is about (spec §4.4). Absent is the workspace's own working
 * directory; an agent session's views pass its tree, and the tool server confines to it. It rides
 * as an ordinary query parameter, so it is part of the cache key without a second map.
 */
async function fsGet<T>(scope: string, what: string, params: Record<string, string> = {}): Promise<T | undefined> {
  const q = new URLSearchParams({ scope, ...params }).toString();
  const key = `${what}?${q}`;
  if (fsCache.has(key)) return fsCache.get(key) as T;
  const r = (await window.harness.bench("GET", `/fs/${what}?${q}`).catch(() => undefined)) as T | undefined;
  if (r !== undefined) fsCache.set(key, r);
  return r;
}
/** One directory of a workspace's tree; no path means its root. */
export const fsTree = (scope: string, path?: string, tree?: string) => fsGet<{ entries?: FsEntry[] }>(scope, "tree", { ...(path ? { path } : {}), ...(tree ? { tree } : {}) });
/** What differs from the branch — and whether it is a git repository at all. */
export const fsChanges = (scope: string, tree?: string) => fsGet<FsChanges>(scope, "changes", tree ? { tree } : {});

/** One file, as the bench's envelope carries it: text, or named and measured when it is not. */
export type FsFile = { text?: string; binary?: true; mime?: string; bytes?: number; etag?: string; notModified?: true };

/**
 * A file's contents. The ETag is kept per path and sent back on the next read, so a file that has
 * not changed costs a 304 and no bytes — and the text already held is what is shown.
 */
const tags = new Map<string, { etag?: string; file: FsFile }>();
export async function fsFile(scope: string, path: string): Promise<FsFile | undefined> {
  const key = `${scope}:${path}`;
  const had = tags.get(key);
  const q = new URLSearchParams({ scope, path, ...(had?.etag ? { etag: had.etag } : {}) }).toString();
  const got = (await window.harness.bench("GET", `/fs/file?${q}`).catch(() => undefined)) as FsFile | undefined;
  if (!got) return had?.file;
  if (got.notModified) return had?.file;
  tags.set(key, { etag: got.etag, file: got });
  return got;
}

/**
 * `/fs/log`, in the tool server's own words (`crates/ide/src/fs/git.rs:191`): the branch's last `n`
 * commits, newest first, each with what it touched. `status` is `--name-status`'s letter and a
 * rename carries where it came from.
 */
export type FsCommit = { hash: string; short: string; subject: string; author: string; at: string; files: { path: string; status: string; from?: string }[] };
/** What this branch has COMMITTED, for the second half of the CHANGES tab. */
export const fsLog = (scope: string, n = 20, tree?: string) => fsGet<{ repo: boolean; commits?: FsCommit[] }>(scope, "log", { n: String(n), ...(tree ? { tree } : {}) });

/** One file's diff against the branch, as the tool server writes it. */
export const fsDiff = (scope: string, path: string, tree?: string) => fsGet<{ diff?: string; binary?: boolean }>(scope, "diff", { path, ...(tree ? { tree } : {}) });

/**
 * What this tree has that main does not: `git diff main...HEAD`, run read-only inside the tree by
 * the bench (spec §4.5). Not cached — it is asked for by a click, and what it answers is exactly
 * the state at that moment.
 */
export const fsAgainstMain = async (scope: string, tree: string): Promise<{ diff?: string; error?: string } | undefined> =>
  (await window.harness.bench("GET", `/fs/against-main?${new URLSearchParams({ scope, tree }).toString()}`).catch(() => undefined)) as
    | { diff?: string; error?: string }
    | undefined;

/** One path's own facts: kind, size, mime. */
export const fsStat = (scope: string, path: string, tree?: string) => fsGet<{ kind?: string; size?: number; mime?: string }>(scope, "stat", { path, ...(tree ? { tree } : {}) });
/** Forget what was read: after a write, or when a workspace is reopened. */
export const forgetFs = () => (fsCache.clear(), tags.clear());
