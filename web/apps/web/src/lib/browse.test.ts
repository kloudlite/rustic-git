import { describe, expect, mock, test } from "bun:test";

// `browse` is `server-only`; that guard throws outside a server component, so it is
// stubbed out before the module loads.
mock.module("server-only", () => ({}));
const { logPath } = await import("./browse");

describe("logPath", () => {
  test("asks the server for a count with the name it reads", () => {
    // `?page=` was sent for a long time and ignored: the server only reads `n`.
    expect(logPath("alice", "web", "abc123", 41)).toBe("/api/alice/web/log/abc123?n=41");
  });

  test("escapes every segment", () => {
    expect(logPath("a b", "c/d", "e", 1)).toBe("/api/a%20b/c%2Fd/log/e?n=1");
  });
});
