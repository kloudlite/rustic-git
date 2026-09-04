import { expect, test } from "bun:test";
import { tightest, byTightest, limitSource } from "./owners-sort";
import type { OwnerDetail, OwnerRow } from "./api";

const row = (owner: string, used: Partial<Record<string, number>>, limit: Partial<Record<string, number>>) =>
  ({ owner, isTeam: true, used, limit } as unknown as OwnerRow);

test("the tightest dimension is the highest ratio, named", () => {
  const r = row("acme", { workspaces: 19, cpu: 4 }, { workspaces: 20, cpu: 32 });
  expect(tightest(r)).toEqual({ dim: "workspaces", used: 19, limit: 20, percent: 95 });
});

test("a zero limit is not the tightest dimension", () => {
  // A dimension with a 0 limit would be 100% under a naive ratio and would pin every owner to
  // the top of a table sorted by pressure.
  const r = row("idle", { workspaces: 0, cpu: 2 }, { workspaces: 0, cpu: 8 });
  expect(tightest(r).dim).toBe("cpu");
});

test("owners sort by pressure, so the one about to hit a wall is first", () => {
  const rows = [
    row("calm", { workspaces: 1 }, { workspaces: 20 }),
    row("acme", { workspaces: 19 }, { workspaces: 20 }),
  ];
  expect(byTightest(rows).map((r) => r.owner)).toEqual(["acme", "calm"]);
});

test("a limit is chipped by where it came from", () => {
  const own = { owner: "acme", source: "own" } as unknown as OwnerDetail;
  const fallback = { owner: "sana", source: "default" } as unknown as OwnerDetail;
  expect(limitSource(own)).toBe("own");
  expect(limitSource(fallback)).toBe("default");
});
