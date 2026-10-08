import { expect, test } from "bun:test";
import type { Entry } from "./components/Transcript.tsx";
import { foldRepeats, foldRetries, foldRetry } from "./retry.ts";

const tool = (id: string, name: string, status: "running" | "ok" | "error", summary = "x"): Entry => ({ kind: "tool", id, name, summary, status });

test("a failed bash then bash is one entry with retries 1; a second failure replaces again", () => {
  const a = foldRetry([tool("1", "bash", "error")], tool("2", "bash", "running"));
  expect(a).toEqual([{ ...tool("2", "bash", "running"), retries: 1 } as Entry]);
  const b = foldRetry([{ ...(a[0] as any), status: "error" }], tool("3", "bash", "running"));
  expect(b.length).toBe(1);
  expect((b[0] as any).retries).toBe(2);
});

test("a different tool, a user message between, or an ok row keep both", () => {
  expect(foldRetry([tool("1", "bash", "error")], tool("2", "read", "running")).length).toBe(2);
  expect(foldRetry([tool("1", "bash", "error"), { kind: "user", text: "again" }], tool("2", "bash", "running")).length).toBe(3);
  expect(foldRetry([tool("1", "bash", "ok")], tool("2", "bash", "running")).length).toBe(2);
});

test("consecutive identical codemode inner calls collapse into the latest with a count", () => {
  const out = foldRepeats([tool("p", "codemode", "running"), tool("p/1", "env_get", "ok", "e1"), tool("p/2", "env_get", "ok", "e1"), tool("p/3", "env_get", "running", "e1"), tool("p/4", "read", "ok", "f")]);
  expect(out.map((e: any) => [e.id, e.repeats])).toEqual([["p", undefined], ["p/3", 3], ["p/4", undefined]]);
});

test("the reload fold replaces a failed row once its status is known", () => {
  const out = foldRetries([tool("1", "bash", "error"), tool("2", "bash", "ok")]);
  expect(out.length).toBe(1);
  expect((out[0] as any).retries).toBe(1);
});
