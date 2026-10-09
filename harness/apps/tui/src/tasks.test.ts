import { expect, test } from "bun:test";
import type { BoardTask, Message } from "@kloudlite-tui/backend";
import { doneTasks, hhmm, planRows, uptime } from "./tasks.ts";

const T = (id: string, o: Partial<BoardTask> = {}): BoardTask => ({ id, title: id, priority: 3, dependsOn: [], state: "queued", created: Number(id.slice(1)), ...o });
const NOW = Date.parse("2026-10-09T12:50:00Z");
const M = (id: string, o: Partial<Message>): Message => ({ id, from: "main", to: "w1", text: "hi", at: "2026-10-09T12:30:00Z", ...o });
const names = (k: string) => ({ main: "main", w1: "todo-demo", w2: "api-ws" })[k] ?? k;
const text = (rows: { text: string; right?: string }[]) => rows.map((r) => (r.right ? `${r.text} | ${r.right}` : r.text));

test("main view: every session with open work, tree under the first dependency, all waits named", () => {
  const rows = planRows(
    [
      { session: "main", tasks: [T("T1", { state: "running" }), T("T2", { dependsOn: ["T1"] }), T("T3", { dependsOn: ["T1", "T4"] }), T("T4", { state: "running" })] },
      { session: "w1", tasks: [T("T1", { state: "done" }), T("T2", { dependsOn: ["T1"], state: "running" })] },
      { session: "w2", tasks: [T("T1", { state: "done" })] },
    ],
    [],
    names,
    "main",
    NOW,
    (k) => (k === "w1" ? "working" : ""),
  );
  expect(text(rows)).toEqual([
    "main",
    "● T1 T1 | working",
    "├─ ○ T2 T2 | waits T1",
    "└─ ○ T3 T3 | waits T1, T4",
    "● T4 T4 | working",
    "todo-demo | working",
    "✓ T1 T1",
    "└─ ● T2 T2 | working",
    "✓ 2 done · ^q",
  ]);
});

test("workspace view shows only its own board", () => {
  const boards = [{ session: "main", tasks: [T("T1", { state: "running" })] }, { session: "w2", tasks: [T("T1")] }];
  expect(text(planRows(boards, [], names, "w2", NOW, () => "idle"))).toEqual(["api-ws | idle", "○ T1 T1 | queued"]);
  expect(planRows(boards, [], names, "w3", NOW)).toEqual([]);
});

test("a message hangs under its task with its reply; unattached ones go to a messages list", () => {
  const boards = [{ session: "main", tasks: [T("T1", { state: "running" }), T("T2", { dependsOn: ["T1"] })] }];
  const msgs = [
    M("m1", { for: "T1", text: "build a todo app" }),
    M("m2", { from: "w1", to: "main", reply: "m1", at: "2026-10-09T12:41:00Z", text: "Todo app built" }),
    M("m3", { to: "w2", text: "ping" }),
  ];
  expect(text(planRows(boards, msgs, names, "main", NOW))).toEqual([
    "main",
    "● T1 T1 | working",
    `│  → todo-demo "build a todo app" | sent ${hhmm(msgs[0]!.at)}`,
    `│  ← todo-demo "Todo app built" | ${hhmm(msgs[1]!.at)}`,
    "└─ ○ T2 T2 | waits T1",
    "messages",
    `  → api-ws "ping" | sent ${hhmm(msgs[2]!.at)}`,
  ]);
});

test("a workspace sees what it was sent in its messages list", () => {
  const rows = planRows([{ session: "w1", tasks: [T("T1", { state: "running" })] }], [M("m1", { for: "T9" })], names, "w1", NOW);
  expect(rows.map((r) => r.text)).toEqual(["todo-demo", "● T1 T1", "messages", `  ← main "hi"`]);
});

test("a board with only an old message and done tasks is not listed, a recent message lists it", () => {
  const boards = [{ session: "main", tasks: [] }, { session: "w1", tasks: [T("T1", { state: "done" })] }];
  expect(planRows(boards, [M("m1", { from: "w1", to: "main", at: "2026-10-09T09:00:00Z" })], names, "main", NOW).map((r) => r.text)).toEqual(["✓ 1 done · ^q"]);
  expect(planRows(boards, [M("m1", { from: "w1", to: "main" })], names, "main", NOW).map((r) => r.text)).toContain("todo-demo");
});

test("fold to ten rows keeps running work and says how many are hidden", () => {
  const tasks = [...Array.from({ length: 14 }, (_, i) => T(`T${i + 1}`)), T("T15", { state: "running" })];
  const rows = planRows([{ session: "main", tasks }], [], names, "main", NOW);
  expect(rows.length).toBe(10);
  expect(text(rows)).toContain("● T15 T15 | working");
  expect(rows.at(-1)!.text).toBe("+7 more · ^g plan");
  expect(planRows([{ session: "main", tasks }], [], names, "main", NOW, () => "", Infinity).length).toBe(16);
});

test("done tasks, newest first, for the ^q screen", () => {
  const boards = [{ session: "main", tasks: [T("T1", { state: "done" })] }, { session: "w1", tasks: [T("T2", { state: "done" }), T("T3")] }];
  expect(doneTasks(boards, names, "main").map((t) => `${t.name} ${t.id}`)).toEqual(["todo-demo T2", "main T1"]);
  expect(doneTasks(boards, names, "w1").map((t) => t.id)).toEqual(["T2"]);
});

test("uptime is empty without a start time", () => expect(uptime(undefined, 0)).toBe(""));
