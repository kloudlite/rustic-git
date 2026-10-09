//! The sidebar's view of main's task board: per workspace the task it is on and what queues behind
//! it. Pure so the grouping and ordering are tested without a renderer.
import type { BoardTask } from "@kloudlite-tui/backend";

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
