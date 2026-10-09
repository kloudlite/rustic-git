import { expect, test } from "bun:test";
import type { SpaceView } from "@kloudlite-tui/backend";
import { fromSpace, isoStart, procName, wsPath, type Workspace } from "./workspaces.ts";

const w = (id: string, parent?: string): Workspace =>
  ({ id, name: id, owner: "karthik", parent, status: "running", ports: [], repo: "r", branch: "main" });

test("path is the workspace, with its parent when ephemeral", () => {
  const ws = [w("a"), w("a1", "a")];
  expect(wsPath(ws, ws[0]!)).toEqual(["a"]);
  expect(wsPath(ws, ws[1]!)).toEqual(["a", "a1"]);
});

const view = (over: Partial<SpaceView> = {}): SpaceView => ({ available: true, user: "me", workspaces: [], environments: [], ...over });
const sw = (id: string, over: Partial<SpaceView["workspaces"][number]> = {}) => ({ id, name: `n-${id}`, owner: "me", state: "ready", ...over });

test("fromSpace: empty view maps to nothing, env index 0", () => {
  expect(fromSpace(view())).toEqual({ workspaces: [], environments: [], envIndex: 0 });
});

test("fromSpace: status follows attachment, then state", () => {
  const { workspaces } = fromSpace(
    view({ workspaces: [sw("a", { attached_environment: "e1" }), sw("b", { state: "stopped" }), sw("c", { state: "creating" }), sw("d"), sw("e", { state: "error" })] }),
  );
  expect(workspaces.map((w) => w.status)).toEqual(["attached", "stopped", "cloning", "running", "stopped"]);
});

test("fromSpace: clones follow their parent directly, a clone of a clone hangs off the root", () => {
  const { workspaces } = fromSpace(
    view({ workspaces: [sw("c1", { parent: "p" }), sw("p"), sw("q"), sw("c2", { parent: "c1", task: "t" }), sw("orphan", { parent: "gone" })] }),
  );
  expect(workspaces.map((w) => [w.id, w.parent])).toEqual([["p", undefined], ["c1", "p"], ["c2", "p"], ["q", undefined], ["orphan", undefined]]);
});

test("fromSpace: service port is the first port, interceptedBy becomes the workspace name", () => {
  const { environments } = fromSpace(
    view({
      workspaces: [sw("w9", { name: "api-gateway" })],
      environments: [{ id: "e", name: "prod", owner: "t", state: "running", services: [{ name: "api", ports: [8080, 9090], interceptedBy: "w9" }, { name: "db", ports: [], interceptedBy: "unknown" }] }],
    }),
  );
  expect(environments[0]!.services).toEqual([
    { name: "api", port: 8080, interceptedBy: "api-gateway" },
    { name: "db", port: 0, interceptedBy: "unknown" },
  ]);
});

test("fromSpace: processes are named by their binary; failed or non-zero exits are crashed", () => {
  const p = (id: string, cmd: string, state: string, extra = {}) => ({ id, cmd, state, logs: [{ text: "l" }], ...extra });
  const { workspaces } = fromSpace(
    view({ workspaces: [sw("a", { processes: [p("1", "/usr/bin/go run ./x", "running"), p("2", "ls", "exited", { exit_code: 0 }), p("3", "make", "exited", { exit_code: 2 }), p("4", "sh", "exited", { exit_code: 0, failed: true }), p("5", "", "exited")] })] }),
  );
  expect(workspaces[0]!.processes!.map((x) => [x.name, x.status, x.code])).toEqual([["go run", "running", undefined], ["ls", "exited", 0], ["make", "crashed", 2], ["sh", "crashed", 0], ["5", "exited", undefined]]);
});

test("fromSpace: the connected environment picks the index; unknown falls back to 0", () => {
  const e = (id: string) => ({ id, name: id, owner: "t", state: "running", services: [] });
  expect(fromSpace(view({ environments: [e("a"), e("b")], connected: "b" })).envIndex).toBe(1);
  expect(fromSpace(view({ environments: [e("a"), e("b")], connected: "zz" })).envIndex).toBe(0);
});

test("procName: the label a person would give the command", () => {
  expect(procName("npm run dev")).toBe("npm run dev");
  expect(procName("cd web && bun run dev")).toBe("bun run dev");
  expect(procName("PORT=3000 node server.js")).toBe("node server.js");
  expect(procName("bash -lc 'npx vite --port 5173'")).toBe("vite");
  expect(procName("/usr/bin/python3 -m http.server 8000")).toBe("python3 http.server");
  expect(procName("")).toBe("");
  expect(procName("x".repeat(40))).toHaveLength(32);
});

test("isoStart: the pod's epoch seconds become RFC 3339; RFC 3339 and missing pass through", () => {
  expect(isoStart("1760000000")).toBe("2025-10-09T08:53:20.000Z");
  expect(isoStart("2026-10-09T00:00:00Z")).toBe("2026-10-09T00:00:00Z");
  expect(isoStart(undefined)).toBeUndefined();
});
