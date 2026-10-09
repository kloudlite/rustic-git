//! The Work panel's view of the task board (components/Work.tsx): main sees the whole plan, per
//! workspace the task it is on and what queues behind it; a workspace session sees only its own
//! tasks plus the processes it runs. Tasks stay out of the sidebar on purpose: the plan belongs to
//! the session doing the work. Pure so the grouping and ordering are tested without a renderer.
import type { BoardTask } from "@kloudlite-tui/backend";
import type { Process } from "./workspaces.ts";

export type TaskGroup = { workspace?: string; current?: BoardTask; queue: (BoardTask & { waits: string[] })[] };

const order = (a: BoardTask, b: BoardTask) => a.priority - b.priority || a.created - b.created;

export function taskGroups(ts: BoardTask[]): { groups: TaskGroup[]; done: number } {
  const open = ts.filter((t) => t.state !== "done");
  const names = [...new Set(open.map((t) => t.workspace))].sort((a, b) => (a === undefined ? 1 : b === undefined ? -1 : a.localeCompare(b)));
  const groups = names.map((workspace) => {
    const mine = open.filter((t) => t.workspace === workspace).sort(order);
    const current = mine.find((t) => t.state === "running") ?? mine.find((t) => t.state === "blocked");
    const queue = mine.filter((t) => t !== current).map((t) => ({ ...t, waits: t.dependsOn.filter((d) => ts.find((x) => x.id === d)?.state !== "done") }));
    return { workspace, current, queue };
  });
  return { groups, done: ts.length - open.length };
}

/** `<1m`, `4m`, `1h 12m`, `2d 3h`; empty when the start time is unknown. */
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

const live = (p: Process) => p.status !== "exited" && p.status !== "crashed";

/** Rows of the Work panel. `ws` undefined = the main session (whole plan, no processes); else the
 * workspace id: its own tasks, then its live processes. */
export function workRows(tasks: BoardTask[], processes: Process[], ws?: string, now = Date.now()): WorkRow[] {
  const { groups, done } = taskGroups(ws === undefined ? tasks : tasks.filter((t) => t.workspace === ws));
  const rows: WorkRow[] = [];
  for (const g of groups) {
    if (g.current) rows.push({ text: `${g.workspace ?? "unassigned"}: ${g.current.id} ${g.current.title} · ${g.current.state}` });
    for (const t of g.queue) rows.push({ text: `  · ${t.id} ${t.title}${t.waits.length ? ` (waits ${t.waits.join(", ")})` : ""}`, dim: true });
  }
  if (done > 0) rows.push({ text: `${done} done`, dim: true });
  if (ws !== undefined)
    for (const p of processes.filter(live)) rows.push({ text: `▸ ${p.name}  ${p.command}  ${uptime(p.startedAt, now)}`.trimEnd() });
  return rows;
}
