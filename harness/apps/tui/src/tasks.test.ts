import { expect, test } from "bun:test";
import type { BoardTask } from "@kloudlite-tui/backend";
import { taskGroups, uptime, workRows } from "./tasks.ts";

const T = (id: string, o: Partial<BoardTask> = {}): BoardTask => ({ id, title: id, priority: 3, dependsOn: [], state: "queued", created: Number(id.slice(1)), ...o });

test("groups by workspace with unassigned last, current first, queue by priority then age", () => {
  const { groups, done } = taskGroups([
    T("T1", { workspace: "b", state: "done" }),
    T("T2", { workspace: "b", state: "running" }),
    T("T3", { workspace: "b", priority: 2, dependsOn: ["T2", "T1"] }),
    T("T4", { workspace: "b" }),
    T("T5", { workspace: "a", state: "blocked" }),
    T("T6", { workspace: "a", state: "failed" }),
    T("T7"),
  ]);
  expect(done).toBe(1);
  expect(groups.map((g) => g.workspace)).toEqual(["a", "b", undefined]);
  expect(groups[0]!.current?.id).toBe("T5");
  expect(groups[0]!.queue.map((t) => `${t.id}:${t.state}`)).toEqual(["T6:failed"]);
  expect(groups[1]!.current?.id).toBe("T2");
  expect(groups[1]!.queue.map((t) => [t.id, t.waits])).toEqual([["T3", ["T2"]], ["T4", []]]);
  expect(groups[2]!.current).toBeUndefined();
  expect(groups[2]!.queue.map((t) => t.id)).toEqual(["T7"]);
});

test("no tasks, no groups", () => expect(taskGroups([])).toEqual({ groups: [], done: 0 }));

const board = [
  T("T1", { workspace: "a", state: "done" }),
  T("T2", { workspace: "a", state: "running" }),
  T("T3", { workspace: "a", dependsOn: ["T2"] }),
  T("T4", { workspace: "b" }),
  T("T5", { workspace: "b", state: "running" }),
  T("T6"),
];

test("workRows main: the whole plan, waits, unassigned, the done task", () => {
  expect(workRows(board).map((r) => r.text)).toEqual([
    "a: T2 T2 · running",
    "  · T3 T3 (waits T2)",
    "b: T5 T5 · running",
    "  · T4 T4",
    "  · T6 T6",
    "✓ T1 T1",
  ]);
});

test("workRows workspace: only its tasks; several done show the last one and a count", () => {
  expect(workRows(board, "a").map((r) => r.text)).toEqual(["a: T2 T2 · running", "  · T3 T3 (waits T2)", "✓ T1 T1"]);
  expect(workRows(board, "zzz")).toEqual([]);
  const many = [T("T1", { state: "done" }), T("T2", { state: "done" }), T("T3", { state: "done" })];
  expect(workRows(many).map((r) => r.text)).toEqual(["✓ T3 T3 (+2 more done)"]);
});

test("uptime is empty without a start time", () => expect(uptime(undefined, 0)).toBe(""));
