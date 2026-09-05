import { describe, expect, test } from "bun:test";
import { fetchRetrying } from "./fetch-retry";

function stub(statuses: number[]) {
  const calls: string[] = [];
  const original = globalThis.fetch;
  globalThis.fetch = (async (url: string) => {
    calls.push(url);
    return new Response("", { status: statuses[calls.length - 1] ?? 200 });
  }) as typeof fetch;
  return { calls, restore: () => { globalThis.fetch = original; } };
}

describe("fetchRetrying", () => {
  // Catches: a roll's transient 503 reaching the page instead of being retried.
  test("a GET that answers 503 is asked once more", async () => {
    const s = stub([503]);
    const res = await fetchRetrying("http://x/y", {});
    s.restore();
    expect(res.status).toBe(200);
    expect(s.calls.length).toBe(2);
  });

  // Catches: replaying a mutation — a second create is a second object.
  test("a POST is never retried", async () => {
    const s = stub([503]);
    const res = await fetchRetrying("http://x/y", { method: "POST" });
    s.restore();
    expect(res.status).toBe(503);
    expect(s.calls.length).toBe(1);
  });
});
