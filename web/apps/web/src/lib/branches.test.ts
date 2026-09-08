import { describe, expect, test } from "bun:test";
import { orderBranches, protectedBy } from "@/lib/branches";
import type { Ref } from "@/lib/browse";

const ref = (name: string, kind: Ref["kind"] = "branch"): Ref =>
  ({ name, oid: "a".repeat(40), kind });

describe("orderBranches", () => {
  test("default first, then alphabetically", () => {
    const out = orderBranches([
      ref("refs/heads/zed"),
      ref("refs/heads/alpha"),
      ref("refs/heads/main"),
    ]);
    expect(out.map((r) => r.name)).toEqual([
      "refs/heads/main",
      "refs/heads/alpha",
      "refs/heads/zed",
    ]);
  });

  test("drops tags and anything not under refs/heads", () => {
    const out = orderBranches([ref("refs/tags/v1", "tag"), ref("refs/heads/x")]);
    expect(out.map((r) => r.name)).toEqual(["refs/heads/x"]);
  });

  test("no default branch present is just alphabetical", () => {
    const out = orderBranches([ref("refs/heads/b"), ref("refs/heads/a")]);
    expect(out.map((r) => r.name)).toEqual(["refs/heads/a", "refs/heads/b"]);
  });
});

describe("protectedBy", () => {
  const rules = [
    { pattern: "main", no_force: true, no_delete: true },
    { pattern: "release/*", no_force: true, no_delete: true },
    { pattern: "wip", no_force: true, no_delete: false },
  ];

  test("exact pattern matches the whole name only", () => {
    expect(protectedBy(rules, "main")).toBe("main");
    expect(protectedBy(rules, "mainline")).toBeUndefined();
  });

  test("a trailing star is a prefix match", () => {
    expect(protectedBy(rules, "release/1.2")).toBe("release/*");
    expect(protectedBy(rules, "releases/1.2")).toBeUndefined();
  });

  test("a rule that does not forbid deletion protects nothing", () => {
    expect(protectedBy(rules, "wip")).toBeUndefined();
  });

  test("no rules, no protection", () => {
    expect(protectedBy([], "main")).toBeUndefined();
  });
});
