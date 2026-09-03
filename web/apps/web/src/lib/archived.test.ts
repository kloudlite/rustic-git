import { expect, test } from "bun:test";
import { archivedRows, deleteVolumeCopy, keptSnapshotsCopy } from "@/lib/archived";
import type { ApiVolumeSummary } from "@/lib/api";

const vol = (v: Partial<ApiVolumeSummary>): ApiVolumeSummary => ({
  name: "v1",
  kind: "workspace",
  volume: "v1",
  display_name: "v1",
  deleted: true,
  snapshots: 1,
  last_push_at: null,
  ...v,
});

test("only volumes whose source is gone and that still hold snapshots", () => {
  const rows = archivedRows([
    vol({ name: "live", deleted: false, snapshots: 3 }),
    vol({ name: "empty", snapshots: 0 }),
    vol({ name: "keep", snapshots: 2 }),
  ]);
  expect(rows.map((r) => r.id)).toEqual(["keep"]);
});

test("newest push first, and an unpushed row sorts last rather than throwing", () => {
  const rows = archivedRows([
    vol({ name: "old", last_push_at: "2026-01-01T00:00:00Z" }),
    vol({ name: "none" }),
    vol({ name: "new", last_push_at: "2026-09-01T00:00:00Z" }),
  ]);
  expect(rows.map((r) => r.id)).toEqual(["new", "old", "none"]);
});

test("a display name equal to the id means no push ever recorded one", () => {
  const [anon, named] = archivedRows([
    vol({ name: "abc", display_name: "abc" }),
    vol({ name: "def", display_name: "api", last_push_at: "2026-09-01T00:00:00Z" }),
  ]).sort((a, b) => a.id.localeCompare(b.id));
  expect(anon.named).toBe(false);
  expect(named.named).toBe(true);
});

test("delete copy counts what is lost", () => {
  expect(deleteVolumeCopy(1)).toBe("Deletes 1 snapshot. This cannot be undone.");
  expect(deleteVolumeCopy(4)).toBe("Deletes 4 snapshots. This cannot be undone.");
});

test("a live delete says what it keeps, counted when the count is at hand", () => {
  expect(keptSnapshotsCopy(3)).toBe(
    "Your 3 snapshots stay under Snapshots; unpushed changes are deleted.",
  );
  expect(keptSnapshotsCopy(1)).toBe(
    "Your 1 snapshot stays under Snapshots; unpushed changes are deleted.",
  );
  expect(keptSnapshotsCopy()).toBe(
    "Your snapshots stay under Snapshots; unpushed changes are deleted.",
  );
});
