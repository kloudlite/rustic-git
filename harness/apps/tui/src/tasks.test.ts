import { expect, test } from "bun:test";
import type { BoardTask } from "@kloudlite-tui/backend";
import { doneTasks, planRows, uptime } from "./tasks.ts";

const T = (id: string, o: Partial<BoardTask> = {}): BoardTask => ({ id, title: id, priority: 3, dependsOn: [], state: "queued", created: Number(id.slice(1)), ...o });
const names = (k: string) => ({ main: "main", w1: "todo-demo", w2: "api-ws" })[k] ?? k;
const text = (rows: { text: string; right?: string }[]) => rows.map((r) => (r.right ? `${r.text} | ${r.right}` : r.text));

test("main view: only open work, tree under the first open dependency, finished tasks and boards gone", () => {
  const rows = planRows(
    [
      { session: "main", tasks: [T("T1", { state: "running" }), T("T2", { dependsOn: ["T1"] }), T("T3", { dependsOn: ["T1", "T4"] }), T("T4", { state: "running" })] },
      { session: "w1", tasks: [T("T1", { state: "done" }), T("T2", { dependsOn: ["T1"], state: "running" }), T("T3", { dependsOn: ["T1", "T2"] })] },
      { session: "w2", tasks: [T("T1", { state: "done" })] },
    ],
    names,
    "main",
    (k) => (k === "w1" ? "working" : ""),
  );
  expect(text(rows)).toEqual([
    "main",
    "● T1 T1 | working",
    "├─ ○ T2 T2 | waits T1",
    "└─ ○ T3 T3 | waits T1, T4",
    "● T4 T4 | working",
    "todo-demo | working",
    "● T2 T2 | working",
    "└─ ○ T3 T3 | waits T2",
  ]);
  expect(rows.filter((r) => r.live).map((r) => r.text)).toEqual(["● T1 T1", "● T4 T4", "● T2 T2"]);
});

test("workspace view shows only its own board; a finished board shows nothing", () => {
  const boards = [{ session: "main", tasks: [T("T1", { state: "running" })] }, { session: "w2", tasks: [T("T1")] }, { session: "w1", tasks: [T("T1", { state: "done" })] }];
  expect(text(planRows(boards, names, "w2", () => "idle"))).toEqual(["api-ws | idle", "○ T1 T1 | queued"]);
  expect(planRows(boards, names, "w1")).toEqual([]);
  expect(planRows(boards, names, "w3")).toEqual([]);
});

test("fold keeps running work and says how many are hidden", () => {
  const tasks = [...Array.from({ length: 14 }, (_, i) => T(`T${i + 1}`)), T("T15", { state: "running" })];
  const rows = planRows([{ session: "main", tasks }], names, "main");
  expect(rows.length).toBe(10);
  expect(text(rows)).toContain("● T15 T15 | working");
  expect(rows.at(-1)!.text).toBe("+7 more · ^g plan");
  expect(planRows([{ session: "main", tasks }], names, "main", () => "", Infinity).length).toBe(16);
});

test("done tasks, newest first, for the ^q screen", () => {
  const boards = [{ session: "main", tasks: [T("T1", { state: "done" })] }, { session: "w1", tasks: [T("T2", { state: "done" }), T("T3")] }];
  expect(doneTasks(boards, names, "main").map((t) => `${t.name} ${t.id}`)).toEqual(["todo-demo T2", "main T1"]);
  expect(doneTasks(boards, names, "w1").map((t) => t.id)).toEqual(["T2"]);
});

test("uptime is empty without a start time", () => expect(uptime(undefined, 0)).toBe(""));

test("a row noted ask:<ws> shows the workspace name before the title", () => {
  const rows = planRows([{ session: "main", tasks: [T("T1", { state: "running", title: "build it", note: "ask:w1" })] }], names, "main");
  expect(text(rows)).toEqual(["main", "● T1 todo-demo: build it | working"]);
});
