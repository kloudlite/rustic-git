import { describe, expect, test } from "bun:test";
import { safeRepoPath, safeSegment } from "./slug";

describe("safeSegment", () => {
  test("accepts what the server accepts", () => {
    for (const s of ["alice", "my-repo", "a.b_c", "A1", "x".repeat(100)]) {
      expect(safeSegment(s)).toBe(s);
    }
  });

  test("rejects anything that would change which path is revalidated", () => {
    for (const s of ["", ".", "..", "a/b", "a[b]", "../etc", "x".repeat(101), "a\u0000b", "café", " alice"]) {
      expect(safeSegment(s)).toBeNull();
    }
  });

  test("accepts every k8s object name, rejects what would move a revalidate", () => {
    // Environment ids are k8s object names (RFC 1123 subdomain), which is a strict subset of the
    // rule above — so guarding `id` with safeSegment refuses no real submission.
    for (const id of ["env-4f2c", "web.staging", "e", "0"]) expect(safeSegment(id)).toBe(id);
    for (const id of ["", "..", "env/../other", "env%2f..", "a\u0000b"]) expect(safeSegment(id)).toBeNull();
  });
});

describe("safeRepoPath", () => {
  test("returns both halves when both pass", () => {
    expect(safeRepoPath("alice", "web")).toEqual({ owner: "alice", repo: "web" });
  });

  test("returns null when either half fails", () => {
    expect(safeRepoPath("a/../b", "web")).toBeNull();
    expect(safeRepoPath("alice", "..")).toBeNull();
  });
});
