import { expect, test } from "bun:test";
import { askFor, baseOf, pickAgentSession } from "./sessions.ts";

const a = (key: string, n: number) => ({ key, n });

test("baseOf names the workspace a session key belongs to", () => {
  expect(baseOf("main")).toBe("main");
  expect(baseOf("ws-a")).toBe("ws-a");
  expect(baseOf("ws-a:agent-1f")).toBe("ws-a");
});

test("an ask shows only in its own workspace's view, oldest first", () => {
  const asks = [a("ws-a", 1), a("main", 2), a("ws-a:agent-1", 3)];
  expect(askFor(asks, "main")?.n).toBe(2);
  expect(askFor([a("ws-a", 1), a("ws-a:agent-1", 3)], "main")).toBeUndefined();
  expect(askFor(asks, "ws-a")?.n).toBe(1);
  expect(askFor(asks.slice(1), "ws-a")?.n).toBe(3);
  expect(askFor(asks, "ws-a:other")?.n).toBe(1);
});

test("a clone row picks its newest agent session, unless it has a regular one", () => {
  const m = (key: string, updated: number) => ({ key, updated });
  expect(pickAgentSession("ws-c", [m("ws-c:agent-a1", 1), m("ws-c:agent-b2", 5), m("ws-d:agent-z", 9)])).toBe("agent-b2");
  expect(pickAgentSession("ws-c", [m("ws-c:agent-a1", 1), m("ws-c", 0)])).toBeUndefined();
  expect(pickAgentSession("ws-c", [])).toBeUndefined();
});
