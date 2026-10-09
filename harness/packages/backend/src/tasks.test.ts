import { expect, test } from "bun:test";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { addTask, blockers, boardText, ready, readTasks, updateTask } from "./tasks.ts";

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

test("ready is per workspace; unassigned only for undefined", () => {
  const f = tmp();
  addTask(f, { title: "a", workspace: "ws-a" });
  addTask(f, { title: "b", workspace: "ws-b" });
  addTask(f, { title: "c" });
  const ts = readTasks(f);
  expect(ready(ts, "ws-a").map((t) => t.id)).toEqual(["T1"]);
  expect(ready(ts).map((t) => t.id)).toEqual(["T3"]);
});

test("boardText shows waits on and the done count", () => {
  const f = tmp();
  addTask(f, { title: "first", workspace: "ws-a" });
  addTask(f, { title: "second", workspace: "ws-a", dependsOn: ["T1"] });
  addTask(f, { title: "old" });
  updateTask(f, "T3", { state: "done" });
  const ts = readTasks(f);
  expect(blockers(ts, ts[1]!)).toEqual(["T1"]);
  const t = boardText(ts);
  expect(t).toContain("T2 [queued] p3 second (ws-a)  waits on T1");
  expect(t.endsWith("1 done")).toBe(true);
  expect(boardText([])).toBe("no tasks");
});
