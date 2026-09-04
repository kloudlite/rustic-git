import { expect, test } from "bun:test";
import { nodeTone, nodeVerbs } from "./nodes";
import type { AdminNode } from "./api";

const n = (p: Partial<AdminNode>): AdminNode => ({ name: "session-0", ready: true, decommission: false, decommissionStatus: null, ...p });

test("a ready node is calm, a draining one is informational, a dead one is critical", () => {
  expect(nodeTone(n({}))).toBe("ok");
  expect(nodeTone(n({ decommission: true, decommissionStatus: "draining running=2 owned=6 copies=4 thin=2" }))).toBe("info");
  expect(nodeTone(n({ ready: false }))).toBe("critical");
});

test("only a drained node offers decommission — the sticky stamp is the gate", () => {
  expect(nodeVerbs(n({}))).toEqual(["drain"]);
  expect(nodeVerbs(n({ decommission: true, decommissionStatus: "draining running=2 owned=6 copies=4 thin=2" }))).toEqual(["undrain"]);
  expect(nodeVerbs(n({ decommission: true, decommissionStatus: "drained 2026-08-28T11:04:22Z" }))).toEqual(["undrain", "decommission"]);
});
