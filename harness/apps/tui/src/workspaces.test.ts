import { expect, test } from "bun:test";
import { wsPath, type Workspace } from "./workspaces.ts";

const w = (id: string, parent?: string): Workspace =>
  ({ id, name: id, owner: "karthik", parent, status: "running", ports: [], repo: "r", branch: "main" });

test("path is the workspace, with its parent when ephemeral", () => {
  const ws = [w("a"), w("a1", "a")];
  expect(wsPath(ws, ws[0]!)).toEqual(["a"]);
  expect(wsPath(ws, ws[1]!)).toEqual(["a", "a1"]);
});
