import { expect, test } from "bun:test";
import { mapChanges, mapTree } from "./podfs.ts";

test("pod tree entries become Files tree nodes under their directory", () => {
  const e = [{ name: "src", kind: "dir" }, { name: "a.ts", kind: "file", ignored: true }];
  expect(mapTree(".", e)).toEqual([
    { name: "src", path: "src", dir: true, ignored: undefined },
    { name: "a.ts", path: "a.ts", dir: false, ignored: true },
  ]);
  expect(mapTree("src", [{ name: "b.ts", kind: "file" }])[0]!.path).toBe("src/b.ts");
});

test("pod change rows become Files changes", () => {
  const rows = [
    { path: "a", index: " ", worktree: "M", additions: 2, deletions: 1 },
    { path: "b", index: "?", worktree: "?" },
    { path: "c", index: "A", worktree: " " },
    { path: "d", index: " ", worktree: "D" },
  ];
  expect(mapChanges(rows).map((c) => c.status)).toEqual(["M", "A", "A", "D"]);
  expect(mapChanges(rows)[0]).toEqual({ path: "a", status: "M", added: 2, removed: 1 });
});
