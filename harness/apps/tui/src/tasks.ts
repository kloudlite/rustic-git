//! The Plan panel's view of the boards (components/Plan.tsx, the ^g screen): one board per session,
//! drawn as a tree under each session's name. Only open work: a task hangs under its first dependency
//! still open, the others show as "waits"; finished tasks leave (^q lists them) and messages are chat
//! cards, never plan rows. Main sees every session with something open; a workspace sees only itself. Pure so grouping, tree and fold are tested without a renderer. Processes live
//! on the Jobs screen (^j), not here.
import type { BoardTask } from "@kloudlite-tui/backend";

export type Board = { session: string; tasks: BoardTask[] };
export type Tone = "accent" | "muted" | "error" | "success" | "warning";
/** One drawn line: guides + text on the left, a state column on the right. */
export type PlanRow = { text: string; right?: string; tone?: Tone; head?: boolean; live?: boolean };

const ICON: Record<BoardTask["state"], [string, Tone]> = {
  running: ["●", "accent"],
  queued: ["○", "muted"],
  blocked: ["!", "error"],
  failed: ["!", "error"],
  done: ["✓", "success"],
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
 * Rows of the Plan panel and the ^g screen: only work still open. `view` is "main" (every session with
 * open work) or a workspace id (its own board). `status` is the header's word (working / needs you /
 * idle). Finished tasks leave (^q lists them) and messages live in the chat as cards, never here.
 * More than `max` rows fold to the running and blocked ones and `+n more · ^g plan`.
 */
export function planRows(
  boards: Board[],
  names: (key: string) => string,
  view: string,
  status: (key: string) => string = () => "",
  max = 10,
): PlanRow[] {
  type Cand = PlanRow & { must?: boolean };
  const out: Cand[] = [];
  for (const b of boards.filter((b) => view === "main" || b.session === view)) {
    const open = b.tasks.filter((t) => t.state !== "done");
    if (!open.length) continue;
    const ids = new Set(open.map((t) => t.id));
    // a task hangs under its first dependency still open; a cycle has no root and is not drawn
    const parentOf = (t: BoardTask) => t.dependsOn.find((d) => ids.has(d));
    const kids = (t: BoardTask) => open.filter((c) => parentOf(c) === t.id);
    out.push({ text: names(b.session), right: status(b.session), head: true });
    const draw = (t: BoardTask, prefix: string, cont: string) => {
      const [icon, tone] = ICON[t.state];
      const blockers = t.state === "queued" ? t.dependsOn.filter((d) => ids.has(d)) : [];
      const right = t.state === "running" ? "working" : blockers.length ? `waits ${blockers.join(", ")}` : t.state;
      out.push({ text: `${prefix}${icon} ${t.id} ${t.note?.startsWith("ask:") ? `${names(t.note.slice(4))}: ${t.title}` : t.title}`, right, tone, live: t.state === "running", must: t.state !== "queued" });
      const k = kids(t);
      k.forEach((c, i) => draw(c, cont + (i === k.length - 1 ? "└─ " : "├─ "), cont + (i === k.length - 1 ? "   " : "│  ")));
    };
    open.filter((t) => !parentOf(t)).forEach((t) => draw(t, "", ""));
  }
  if (out.length <= max) return out;

  // fold: running/blocked rows first, then the rest in order, each with its group header
  const room = max - 1;
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
  return [...out.filter((_, i) => keep.has(i)), { text: `+${hidden} more · ^g plan`, tone: "muted" }];
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
