import { expect, test } from "bun:test";
import { depthOf, parentFor, treePrefixes, wsPath, type Workspace } from "./workspaces.ts";

const w = (id: string, parent?: string): Workspace =>
  ({ id, name: id, owner: "karthik", parent, status: "running", ports: [], repo: "r", branch: "main" });

test("guides nest ephemerals under their workspace", () => {
  const ws = [w("a"), w("a1", "a"), w("a2", "a"), w("b")];
  expect(treePrefixes(ws)).toEqual(["├─ ", "│  ├─ ", "│  └─ ", "└─ "]);
});

test("the tree is exactly three levels: session › workspace › ephemeral", () => {
  const ws = [w("a"), w("a1", "a")];
  expect(depthOf(ws, ws[0]!)).toBe(0);
  expect(depthOf(ws, ws[1]!)).toBe(1);
  // spinning one off an ephemeral hangs it off that ephemeral's workspace
  expect(parentFor(ws, ws[1]!)).toBe("a");
  expect(parentFor(ws, ws[0]!)).toBe("a");
});

test("siblings keep the trunk open", () => {
  const ws = [w("a"), w("a1", "a"), w("a2", "a"), w("b")];
  expect(treePrefixes(ws)).toEqual(["├─ ", "│  ├─ ", "│  └─ ", "└─ "]);
});

test("path is the workspace, with its parent when ephemeral", () => {
  const ws = [w("a"), w("a1", "a")];
  expect(wsPath(ws, ws[0]!)).toEqual(["a"]);
  expect(wsPath(ws, ws[1]!)).toEqual(["a", "a1"]);
});
