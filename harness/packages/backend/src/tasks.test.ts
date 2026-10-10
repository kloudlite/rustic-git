import { expect, test } from "bun:test";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { addTask, blockers, boardTools, boardText, ready, readTasks, taskTools, tasksFile, updateTask } from "./tasks.ts";

const tmp = () => join(mkdtempSync(join(tmpdir(), "kl-tasks-")), "tasks.json");

test("a missing file reads as []", () => expect(readTasks(tmp())).toEqual([]));

test("add assigns T1, T2 and refuses an unknown dependency", () => {
  const f = tmp();
  expect((addTask(f, { title: "a" }) as any).id).toBe("T1");
  expect((addTask(f, { title: "b", dependsOn: ["T1"] }) as any).id).toBe("T2");
  expect(addTask(f, { title: "c", dependsOn: ["T9"] })).toBe("error: unknown task T9");
  expect(readTasks(f).length).toBe(2);
});

test("update refusing a loop or unknown id leaves the file unchanged", () => {
  const f = tmp();
  addTask(f, { title: "a" });
  addTask(f, { title: "b", dependsOn: ["T1"] });
  const before = readFileSync(f, "utf8");
  expect(updateTask(f, "T1", { dependsOn: ["T2"] })).toBe("error: T1 -> T2 -> T1 is a loop");
  expect(updateTask(f, "T1", { dependsOn: ["T1"] })).toBe("error: T1 -> T1 is a loop");
  expect(updateTask(f, "T9", { note: "x" })).toBe("error: unknown task T9");
  expect(readFileSync(f, "utf8")).toBe(before);
});

test("ready orders by priority then created and skips tasks blocked by deps", () => {
  const f = tmp();
  addTask(f, { title: "late", priority: 3 });
  addTask(f, { title: "urgent", priority: 1 });
  addTask(f, { title: "blocked", priority: 1, dependsOn: ["T1"] });
  let ts = readTasks(f);
  expect(ready(ts).map((t) => t.id)).toEqual(["T2", "T1"]);
  updateTask(f, "T1", { state: "done" });
  ts = readTasks(f);
  expect(ready(ts).map((t) => t.id)).toEqual(["T2", "T3"]);
});

test("boardText shows waits on and the done count", () => {
  const f = tmp();
  addTask(f, { title: "first" });
  addTask(f, { title: "second", dependsOn: ["T1"] });
  addTask(f, { title: "old" });
  updateTask(f, "T3", { state: "done" });
  const ts = readTasks(f);
  expect(blockers(ts, ts[1]!)).toEqual(["T1"]);
  const t = boardText(ts);
  expect(t).toContain("T2 [queued] p3 second  waits on T1");
  expect(t.endsWith("1 done")).toBe(true);
  expect(boardText([])).toBe("no tasks");
});

test("tasksFile is per base session", () => {
  expect(tasksFile("main:x", "/d")).toBe("/d/main.json");
  expect(tasksFile("ws-a", "/d")).toBe("/d/ws-a.json");
});

test("two sessions' boards are separate", async () => {
  const d = mkdtempSync(join(tmpdir(), "kl-boards-"));
  const [add] = taskTools(tasksFile("main", d));
  const [wsAdd] = taskTools(tasksFile("ws-a:agent-1", d));
  await add!.run({ title: "m" });
  await wsAdd!.run({ title: "w1" });
  await wsAdd!.run({ title: "w2" });
  expect(readTasks(join(d, "main.json")).map((t) => t.title)).toEqual(["m"]);
  expect(readTasks(join(d, "ws-a.json")).map((t) => [t.id, t.title])).toEqual([["T1", "w1"], ["T2", "w2"]]);
});

test("workspace_tasks reads one board, all boards, a missing board, never main", async () => {
  const d = mkdtempSync(join(tmpdir(), "kl-boards-"));
  await taskTools(tasksFile("main", d))[0]!.run({ title: "m" });
  await taskTools(tasksFile("ws-a", d))[0]!.run({ title: "a1" });
  await taskTools(tasksFile("ws-b", d))[0]!.run({ title: "b1" });
  const [t] = boardTools(d);
  const titles = (r: string) => JSON.parse(r).map((b: any) => [b.workspace, b.tasks.map((x: any) => x.title)]);
  expect(titles(await t!.run({ workspace: "ws-a:agent-1" }))).toEqual([["ws-a:agent-1", ["a1"]]]);
  expect(titles(await t!.run({}))).toEqual([["ws-a", ["a1"]], ["ws-b", ["b1"]]]);
  expect(titles(await t!.run({ workspace: "ws-zzz" }))).toEqual([["ws-zzz", []]]);
});
