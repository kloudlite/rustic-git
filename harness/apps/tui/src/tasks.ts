//! The Work panel's view of the task board (components/Work.tsx): main sees the whole plan, per
//! workspace the task it is on and what queues behind it; a workspace session sees only its own
//! tasks. Processes live on the Jobs screen (^j), not here. Tasks stay out of the sidebar on purpose:
//! the plan belongs to the session doing the work. Pure so the grouping and ordering are tested
//! without a renderer.
import type { BoardTask as Board } from "@kloudlite-tui/backend";

/** A board task as this panel groups it. The backend board is per session now, so `workspace` is unset until the panel is redesigned. */
export type BoardTask = Board & { workspace?: string };

export type TaskGroup = { workspace?: string; current?: BoardTask; queue: (BoardTask & { waits: string[] })[] };

const order = (a: BoardTask, b: BoardTask) => a.priority - b.priority || a.created - b.created;

export function taskGroups(ts: BoardTask[]): { groups: TaskGroup[]; done: number; lastDone?: BoardTask } {
  const open = ts.filter((t) => t.state !== "done");
  const names = [...new Set(open.map((t) => t.workspace))].sort((a, b) => (a === undefined ? 1 : b === undefined ? -1 : a.localeCompare(b)));
  const groups = names.map((workspace) => {
    const mine = open.filter((t) => t.workspace === workspace).sort(order);
    const current = mine.find((t) => t.state === "running") ?? mine.find((t) => t.state === "blocked");
    const queue = mine.filter((t) => t !== current).map((t) => ({ ...t, waits: t.dependsOn.filter((d) => ts.find((x) => x.id === d)?.state !== "done") }));
    return { workspace, current, queue };
  });
  const finished = ts.filter((t) => t.state === "done");
  return { groups, done: finished.length, lastDone: finished.at(-1) };
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

export type WorkRow = { text: string; dim?: boolean };

/** Rows of the Work panel. `ws` undefined = the main session (whole plan); else the workspace id:
 * its own tasks. The newest done task stands in for the finished ones. */
export function workRows(tasks: BoardTask[], ws?: string): WorkRow[] {
  const { groups, done, lastDone } = taskGroups(ws === undefined ? tasks : tasks.filter((t) => t.workspace === ws));
  const rows: WorkRow[] = [];
  for (const g of groups) {
    if (g.current) rows.push({ text: `${g.workspace ?? "unassigned"}: ${g.current.id} ${g.current.title} · ${g.current.state}` });
    for (const t of g.queue) rows.push({ text: `  · ${t.id} ${t.title}${t.waits.length ? ` (waits ${t.waits.join(", ")})` : ""}`, dim: true });
  }
  if (lastDone) rows.push({ text: `✓ ${lastDone.id} ${lastDone.title}${done > 1 ? ` (+${done - 1} more done)` : ""}`, dim: true });
  return rows;
}
