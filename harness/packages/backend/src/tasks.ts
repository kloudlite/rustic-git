//! A session's own task board: one JSON file per session (`~/.kl/tasks/{session}.json`), written only
//! by that session's task tools. Messages between sessions never touch a board. No dispatcher by
//! design: the session decides what runs next, the board only records, orders (priority, then age)
//! and says what is blocked on what. Written tmp +
//! rename like asks.ts so a crash never leaves half a file.
import { mkdirSync, readdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import type { ToolDef } from "@kloudlite-tui/tools";
import type { BoardTask, TaskState } from "./index.ts";

export const tasksDir = () => join(homedir(), ".kl", "tasks");
/** `session` is the BASE key ("main" or the workspace id), the part before the first `:`. */
export const tasksFile = (session: string, dir = tasksDir()) => join(dir, `${session.split(":")[0]}.json`);

const STATES: TaskState[] = ["queued", "running", "blocked", "done", "failed"];

export function readTasks(file: string): BoardTask[] {
  try {
    const v = JSON.parse(readFileSync(file, "utf8"));
    return Array.isArray(v) ? v : [];
  } catch {
    return [];
  }
}

function write(file: string, ts: BoardTask[]): void {
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(`${file}.tmp`, JSON.stringify(ts));
  renameSync(`${file}.tmp`, file);
}

/** Every session's board under `dir`: main first (always), then the others by name, empty ones skipped. */
export function readBoards(dir = tasksDir()): { session: string; tasks: BoardTask[] }[] {
  const out = [{ session: "main", tasks: readTasks(tasksFile("main", dir)) }];
  let names: string[] = [];
  try {
    names = readdirSync(dir).sort();
  } catch {}
  for (const n of names) {
    const tasks = n.endsWith(".json") && n !== "main.json" ? readTasks(join(dir, n)) : [];
    if (tasks.length) out.push({ session: n.slice(0, -5), tasks });
  }
  return out;
}

/** The path from `from` back to `to` through dependsOn, if any. */
function reach(ts: BoardTask[], from: string, to: string, seen = new Set<string>()): string[] | undefined {
  if (from === to) return [from];
  if (seen.has(from)) return undefined;
  seen.add(from);
  for (const d of ts.find((t) => t.id === from)?.dependsOn ?? []) {
    const p = reach(ts, d, to, seen);
    if (p) return [from, ...p];
  }
  return undefined;
}

/** Why `deps` cannot be the dependencies of `id` (which may not exist yet), else undefined. */
function badDeps(ts: BoardTask[], id: string, deps: string[]): string | undefined {
  for (const d of deps) if (!ts.some((t) => t.id === d)) return `error: unknown task ${d}`;
  for (const d of deps) {
    const p = reach(ts, d, id);
    if (p) return `error: ${[id, ...p].join(" -> ")} is a loop`;
  }
  return undefined;
}

export function addTask(file: string, a: { title: string; priority?: number; dependsOn?: string[]; note?: string }): BoardTask | string {
  const ts = readTasks(file);
  const id = `T${Math.max(0, ...ts.map((t) => Number(t.id.slice(1)) || 0)) + 1}`;
  const dependsOn = a.dependsOn ?? [];
  const bad = badDeps(ts, id, dependsOn);
  if (bad) return bad;
  const t: BoardTask = { id, title: a.title, priority: a.priority ?? 3, dependsOn, state: "queued", note: a.note, created: Date.now() };
  write(file, [...ts, t]);
  return t;
}

export function updateTask(file: string, id: string, p: Partial<Pick<BoardTask, "title" | "priority" | "dependsOn" | "state" | "note">>): BoardTask | string {
  const ts = readTasks(file);
  const cur = ts.find((t) => t.id === id);
  if (!cur) return `error: unknown task ${id}`;
  if (p.dependsOn) {
    const bad = badDeps(ts, id, p.dependsOn);
    if (bad) return bad;
  }
  const next = { ...cur, ...Object.fromEntries(Object.entries(p).filter(([, v]) => v !== undefined)) } as BoardTask;
  write(file, ts.map((t) => (t.id === id ? next : t)));
  return next;
}

/** Dependencies of `t` that are not done. A deleted dependency counts as blocking. */
export const blockers = (ts: BoardTask[], t: BoardTask): string[] => t.dependsOn.filter((d) => ts.find((x) => x.id === d)?.state !== "done");

const order = (a: BoardTask, b: BoardTask) => a.priority - b.priority || a.created - b.created;

/** Queued, unblocked tasks, best first. */
export const ready = (ts: BoardTask[]): BoardTask[] => ts.filter((t) => t.state === "queued" && !blockers(ts, t).length).sort(order);

const line = (ts: BoardTask[], t: BoardTask): string => {
  const w = blockers(ts, t);
  return `${t.id} [${t.state}] p${t.priority} ${t.title}${w.length ? `  waits on ${w.join(", ")}` : ""}`;
};

export function boardText(ts: BoardTask[]): string {
  if (!ts.length) return "no tasks";
  const open = ts.filter((t) => t.state !== "done");
  return [...open.sort(order).map((t) => line(ts, t)), `${ts.length - open.length} done`].join("\n");
}

const STR = { type: "string" };
const DEPS = { type: "array", items: STR };

export function taskTools(file: string): ToolDef[] {
  const add: ToolDef = {
    name: "task_add",
    description: "Add a task to your own board. A lower priority number runs first. The default is 3. `depends_on` lists the ids of tasks that must be done first. Returns the task line.",
    inputSchema: { type: "object", properties: { title: STR, priority: { type: "number" }, depends_on: DEPS, note: STR }, required: ["title"] },
    async run(i: { title: string; priority?: number; depends_on?: string[]; note?: string }) {
      const r = addTask(file, { title: i.title, priority: i.priority, dependsOn: i.depends_on, note: i.note });
      return typeof r === "string" ? r : line(readTasks(file), r);
    },
  };
  const update: ToolDef = {
    name: "task_update",
    description: "Change a task. You can change the title, the priority, `depends_on`, the state or the note. The tool refuses a dependency loop or an unknown id. Then the board does not change. Returns the task line.",
    inputSchema: { type: "object", properties: { id: STR, title: STR, priority: { type: "number" }, depends_on: DEPS, state: { type: "string", enum: STATES }, note: STR }, required: ["id"] },
    async run(i: { id: string; title?: string; priority?: number; depends_on?: string[]; state?: TaskState; note?: string }) {
      const { id, depends_on, ...rest } = i;
      const r = updateTask(file, id, { ...rest, dependsOn: depends_on });
      return typeof r === "string" ? r : line(readTasks(file), r);
    },
  };
  const list: ToolDef = {
    name: "task_list",
    description: "Show the task board. It lists your open tasks, with what each task waits on. At the end it gives the count of done tasks.",
    inputSchema: { type: "object", properties: {} },
    async run() {
      return boardText(readTasks(file));
    },
  };
  return [add, update, list];
}
