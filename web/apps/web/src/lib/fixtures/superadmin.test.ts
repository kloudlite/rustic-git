import { expect, test } from "bun:test";
import { fixtureFor } from "./superadmin";
import type { OwnerRow } from "@/lib/api";
import type { HistorySeries } from "@/lib/history";

test("every route the eight rebuilt pages read has a seeded answer", () => {
  for (const p of [
    "/admin/overview",
    "/admin/owners",
    "/admin/owners/acme",
    "/admin/clusters",
    "/admin/clusters/centralindia-k3s",
    "/admin/workloads",
    "/admin/monitoring/signals",
    "/admin/audit?limit=50",
    "/admin/settings/schema",
    "/admin/settings/central",
    "/admin/settings/clusters/centralindia-k3s",
    "/admin/quota-requests?state=pending",
    "/admin/history/live_workspaces?range=7d&step=1d",
    "/admin/history/pool_used?range=7d&step=1d&region=westeurope-k3s",
    "/admin/history/events?limit=5",
    "/v1/regions",
    "/v1/quota?owner=default-user",
    "/api/admin/superadmins",
  ]) {
    expect(fixtureFor(p)).toBeDefined();
  }
});

test("an unseeded path answers undefined so the caller falls through to the real api", () => {
  // The guard must never turn a route nobody seeded into an empty 200: a blank section in a
  // screenshot would read as "the console is broken", not as "the fixture is thin".
  expect(fixtureFor("/admin/something-nobody-seeded")).toBeUndefined();
  expect(fixtureFor("/admin/history/no_such_series?range=7d")).toBeUndefined();
  expect(fixtureFor("/admin/owners/nobody")).toBeUndefined();
  expect(fixtureFor("/v1/repos?owner=acme")).toBeUndefined();
});

test("the seed is realistic enough to exercise every tone", () => {
  const owners = fixtureFor("/admin/owners") as OwnerRow[];
  const ratios = owners.map((o) => o.used.workspaces / o.limit.workspaces);
  expect(Math.max(...ratios)).toBeGreaterThanOrEqual(1); // a critical bar
  expect(ratios.some((r) => r >= 0.8 && r < 1)).toBe(true); // a warn bar
  expect(Math.min(...ratios)).toBeLessThan(0.8); // a calm bar
});

test("a series carries seven daily points and a summary that matches them", () => {
  const s = fixtureFor("/admin/history/live_workspaces?range=7d&step=1d") as Omit<HistorySeries, "available">;
  expect(s.series).toHaveLength(7);
  expect(s.summary.last).toBe(s.series[6].value);
  expect(s.summary.delta).toBe(s.series[6].value - s.series[0].value);
  expect(s.summary.max).toBe(Math.max(...s.series.map((p) => p.value)));
});

test("a region's gauge differs from the fleet's, so two region cards are not one line twice", () => {
  const central = fixtureFor("/admin/history/pool_used?range=7d&step=1d&region=centralindia-k3s") as HistorySeries;
  const eu = fixtureFor("/admin/history/pool_used?range=7d&step=1d&region=westeurope-k3s") as HistorySeries;
  expect(eu.summary.last).toBeLessThan(central.summary.last);
});

test("the requests queue narrows by state and by owner, as the pages ask it to", () => {
  const pending = fixtureFor("/admin/quota-requests?state=pending") as { state: string }[];
  expect(pending.length).toBeGreaterThan(0);
  expect(pending.every((r) => r.state === "pending")).toBe(true);
  const acme = fixtureFor("/admin/quota-requests?owner=acme") as { owner: string }[];
  expect(acme.every((r) => r.owner === "acme")).toBe(true);
});
