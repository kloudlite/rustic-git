import { expect, test } from "bun:test";
import { filterQueue, summaryLine, contextLine } from "./request-queue";
import type { RequestDoc } from "./api";
import type { OwnerRow } from "./api";
import type { QuotaDim } from "./quota";

const base: RequestDoc = {
  id: "r1",
  owner: "acme",
  kind: "quota",
  requestedBy: "meera",
  reason: "six contractors",
  quota: { workspaces: 40 },
  state: "pending",
  createdAt: "2026-09-02T10:00:00Z",
};
const now = new Date("2026-09-04T10:00:00Z").getTime();

test("the age filter keeps rows OLDER than the cutoff, not newer", () => {
  const fresh = { ...base, id: "r2", createdAt: "2026-09-04T09:00:00Z" };
  const rows = [base, fresh];
  expect(filterQueue(rows, { kind: "any", ownerType: "any", age: "1d" }, now).map((r) => r.id)).toEqual(["r1"]);
  expect(filterQueue(rows, { kind: "any", ownerType: "any", age: "any" }, now).length).toBe(2);
});

test("the kind filter is exact, so an access request never shows under quota", () => {
  const access: RequestDoc = { ...base, id: "r3", kind: "access", quota: undefined, access: { team: "acme", role: "admin" } };
  expect(filterQueue([base, access], { kind: "access", ownerType: "any", age: "any" }, now).map((r) => r.id)).toEqual(["r3"]);
});

test("owner type comes from the owners list, and keeps every row when that read degraded", () => {
  const priya: RequestDoc = { ...base, id: "r4", owner: "priya" };
  const teams = new Set(["acme"]);
  expect(filterQueue([base, priya], { kind: "any", ownerType: "team", age: "any" }, now, teams).map((r) => r.id)).toEqual(["r1"]);
  expect(filterQueue([base, priya], { kind: "any", ownerType: "person", age: "any" }, now, teams).map((r) => r.id)).toEqual(["r4"]);
  expect(filterQueue([base, priya], { kind: "any", ownerType: "team", age: "any" }, now).length).toBe(2);
});

test("each kind gets its own one-line summary", () => {
  expect(summaryLine(base)).toBe("Raise workspaces to 40");
  expect(summaryLine({ ...base, kind: "access", quota: undefined, access: { team: "acme", role: "admin" } })).toBe("Become admin on acme");
  expect(summaryLine({ ...base, kind: "region", quota: undefined, region: { region: "westeurope-k3s" } })).toBe("Enable westeurope-k3s");
  expect(summaryLine({ ...base, kind: "other", quota: undefined, other: { title: "Restore a snapshot", body: "deleted snap-4c1e" } })).toBe(
    "Restore a snapshot",
  );
});

test("the muted second line carries the kind's own context", () => {
  const dims = (n: number) => Object.fromEntries(["workspaces", "environments", "snapshots", "diskGb", "cpu", "memoryGb"].map((d) => [d, n])) as Record<QuotaDim, number>;
  const usage: OwnerRow = { owner: "acme", isTeam: true, used: { ...dims(0), workspaces: 19 }, limit: { ...dims(0), workspaces: 20 }, source: "own", pending: true };
  expect(contextLine(base, usage)).toBe("19 / 20 in use");
  // No usage read (the owners call degraded) must not print "undefined / undefined".
  expect(contextLine(base, undefined)).toBe("current usage unavailable");
  expect(contextLine({ ...base, kind: "other", quota: undefined, other: { title: "t", body: "line one\nline two" } }, undefined)).toBe("line one");
});
