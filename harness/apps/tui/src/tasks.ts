//! The Plan panel's view of the boards (components/Plan.tsx, the ^g screen): one board per session,
//! drawn as a tree under each session's name. A task hangs under its FIRST dependency; the other
//! dependencies show as "waits". Messages between sessions hang under the sender's task they were
//! sent for, and their answers under that. Main sees every session with something open; a workspace
//! sees only itself. Pure so grouping, tree and fold are tested without a renderer. Processes live
//! on the Jobs screen (^j), not here.
import type { BoardTask, Message } from "@kloudlite-tui/backend";

export type Board = { session: string; tasks: BoardTask[] };
export type Tone = "accent" | "muted" | "error" | "success" | "warning";
/** One drawn line: guides + text on the left, a state column on the right. */
export type PlanRow = { text: string; right?: string; tone?: Tone; head?: boolean };

const ICON: Record<BoardTask["state"], [string, Tone]> = {
  running: ["●", "accent"],
  queued: ["○", "muted"],
  blocked: ["!", "error"],
  failed: ["!", "error"],
  done: ["✓", "success"],
};
const HOUR = 3_600_000;

export const hhmm = (at: string) => {
  const d = new Date(at);
  return Number.isNaN(+d) ? "" : `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
};

const quote = (text: string) => {
  const line = text.split("\n")[0]!.trim();
  return `"${line.length > 24 ? `${line.slice(0, 23)}…` : line}"`;
};

/** The finished tasks of the view, newest first: the ^q screen. */
export type DoneTask = BoardTask & { session: string; name: string };
export function doneTasks(boards: Board[], names: (key: string) => string, view: string): DoneTask[] {
  return boards
    .filter((b) => view === "main" || b.session === view)
    .flatMap((b) => b.tasks.filter((t) => t.state === "done").map((t) => ({ ...t, session: b.session, name: names(b.session) })))
    .sort((a, b) => b.created - a.created);
}

/**
 * Rows of the Plan panel. `view` is "main" (every session worth showing) or a workspace id (its own
 * board, and the messages it sent or got). `status` is the header's word (working / needs you / idle).
 * More than `max` rows fold to the running and blocked ones, their parents, and `+n more · ^g plan`.
 */
export function planRows(
  boards: Board[],
  messages: Message[],
  names: (key: string) => string,
  view: string,
  now: number,
  status: (key: string) => string = () => "",
  max = 10,
): PlanRow[] {
  const mine = (m: Message) => m.from === view || m.to === view;
  const msgs = messages.filter((m) => view === "main" || mine(m));
  const recent = (s: string) => msgs.some((m) => (m.from === s || m.to === s) && now - Date.parse(m.at) < HOUR);
  const inView = boards.filter((b) => (view === "main" ? true : b.session === view));
  const doneCount = inView.reduce((n, b) => n + b.tasks.filter((t) => t.state === "done").length, 0);

  type Cand = PlanRow & { must?: boolean };
  const out: Cand[] = [];
  for (const b of inView) {
    const open = b.tasks.some((t) => t.state !== "done");
    if (view === "main" && b.session !== "main" && !open && !recent(b.session)) continue;
    const g: Cand[] = [];
    const byId = new Map(b.tasks.map((t) => [t.id, t]));
    const parentOf = (t: BoardTask) => byId.get(t.dependsOn[0] ?? "");
    const kids = (t: BoardTask) => b.tasks.filter((c) => parentOf(c) === t);
    // a done task is context only while something unfinished hangs under it
    const live = (t: BoardTask): boolean => t.state !== "done" || kids(t).some(live);
    const placed = new Set<string>();
    const sentFor = (t: BoardTask) => msgs.filter((m) => m.from === b.session && m.for === t.id);
    const note = (m: Message, prefix: string) => {
      const out_ = m.from === b.session;
      const other = names(out_ ? m.to : m.from);
      const when = hhmm(m.at);
      g.push({ text: `${prefix}${out_ ? "→" : "←"} ${other} ${quote(m.text)}`, right: out_ ? `sent ${when}` : when, tone: "muted" });
      placed.add(m.id);
    };
    const draw = (t: BoardTask, prefix: string, cont: string) => {
      const [icon, tone] = ICON[t.state];
      const blockers = t.state === "queued" ? t.dependsOn.filter((d) => byId.get(d)?.state !== "done") : [];
      const right = t.state === "done" ? "" : t.state === "running" ? "working" : blockers.length ? `waits ${blockers.join(", ")}` : t.state;
      const shown = kids(t).filter(live);
      g.push({ text: `${prefix}${icon} ${t.id} ${t.title}`, right, tone, must: t.state === "running" || t.state === "blocked" || t.state === "failed" });
      const under = shown.length ? "│  " : "   ";
      for (const sent of sentFor(t)) {
        note(sent, cont + under);
        for (const r of msgs.filter((m) => m.reply === sent.id)) note(r, cont + under);
      }
      shown.forEach((c, i) => draw(c, cont + (i === shown.length - 1 ? "└─ " : "├─ "), cont + (i === shown.length - 1 ? "   " : "│  ")));
    };
    b.tasks.filter((t) => !parentOf(t) && live(t)).forEach((t) => draw(t, "", ""));
    // no task to hang under: the sender's last three, plus in a workspace view what it was sent
    const loose = msgs.filter((m) => !placed.has(m.id) && (m.from === b.session || (view !== "main" && m.to === b.session)));
    if (loose.length) {
      g.push({ text: "messages", tone: "muted" });
      for (const m of loose.slice(-3)) note(m, "  ");
    }
    if (!g.length) continue;
    out.push({ text: names(b.session), right: status(b.session), head: true }, ...g);
  }
  const tail: PlanRow[] = doneCount ? [{ text: `✓ ${doneCount} done · ^q`, tone: "muted" }] : [];
  if (out.length + tail.length <= max) return [...out, ...tail];

  // fold: running/blocked rows first, then the rest in order, each with its group header
  const room = max - 1 - tail.length;
  const headOf = out.map((_, i) => out.findLastIndex((r, j) => j <= i && r.head));
  const order = out.map((_, i) => i).filter((i) => !out[i]!.head).sort((a, b) => Number(!!out[b]!.must) - Number(!!out[a]!.must) || a - b);
  const keep = new Set<number>();
  let used = 0;
  for (const i of order) {
    const cost = keep.has(headOf[i]!) ? 1 : 2;
    if (used + cost > room) continue;
    used += cost;
    keep.add(i);
    keep.add(headOf[i]!);
  }
  const hidden = order.length - [...keep].filter((i) => !out[i]!.head).length;
  return [...out.filter((_, i) => keep.has(i)), { text: `+${hidden} more · ^g plan`, tone: "muted" }, ...tail];
}

/** `<1m`, `4m`, `1h 12m`, `2d 3h`; empty when the start time is unknown (callers say running/exited). */
export function uptime(startedAt: string | undefined, now: number): string {
  const t = startedAt ? Date.parse(startedAt) : NaN;
  if (Number.isNaN(t)) return "";
  const m = Math.max(0, Math.floor((now - t) / 60_000));
  if (m < 1) return "<1m";
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}
