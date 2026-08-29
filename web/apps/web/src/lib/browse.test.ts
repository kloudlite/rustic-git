import { describe, expect, mock, test } from "bun:test";

// `browse` is `server-only`; that guard throws outside a server component, so it is
// stubbed out before the module loads.
mock.module("server-only", () => ({}));
const { decodeBlob, defaultBranch, logPath, resolveRef, shortRef } = await import("./browse");

describe("logPath", () => {
  test("asks the server for a count with the name it reads", () => {
    // `?page=` was sent for a long time and ignored: the server only reads `n`.
    expect(logPath("alice", "web", "abc123", 41)).toBe("/api/alice/web/log/abc123?n=41");
  });

  test("escapes every segment", () => {
    expect(logPath("a b", "c/d", "e", 1)).toBe("/api/a%20b/c%2Fd/log/e?n=1");
  });
});

const refs = [
  { name: "refs/heads/dev", oid: "a".repeat(40), kind: "branch" as const },
  { name: "refs/heads/main", oid: "b".repeat(40), kind: "branch" as const },
  { name: "refs/tags/v1", oid: "c".repeat(40), kind: "tag" as const },
];

describe("defaultBranch", () => {
  test("main, then master, then whatever branch exists", () => {
    expect(defaultBranch(refs)?.name).toBe("refs/heads/main");
    expect(defaultBranch(refs.filter((r) => r.name !== "refs/heads/main"))?.name).toBe("refs/heads/dev");
    expect(defaultBranch(refs.filter((r) => r.kind === "tag"))).toBeUndefined();
  });
});

describe("resolveRef", () => {
  test("a short name, an oid, or the default", () => {
    expect(resolveRef(refs, "v1")?.kind).toBe("tag");
    expect(resolveRef(refs, "d".repeat(40))).toEqual({ name: "d".repeat(40), oid: "d".repeat(40), kind: "commit" });
    // A deleted branch someone still links to falls back rather than 404s.
    expect(resolveRef(refs, "gone")?.name).toBe("refs/heads/main");
    expect(resolveRef(refs)?.name).toBe("refs/heads/main");
    expect(shortRef("refs/tags/v1")).toBe("v1");
  });
});

describe("decodeBlob", () => {
  test("text decodes, a NUL byte is binary", () => {
    expect(decodeBlob({ oid: "x", bytes_base64: Buffer.from("hi").toString("base64"), truncated: false })).toEqual({ text: "hi", binary: false });
    expect(decodeBlob({ oid: "x", bytes_base64: Buffer.from([104, 0, 105]).toString("base64"), truncated: false })).toEqual({ binary: true });
  });
});
