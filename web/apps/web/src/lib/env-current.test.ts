import { describe, expect, test } from "bun:test";
import { envCurrent } from "./env-current";

/** Newest first, exactly as `/v1/volumes/{name}/history` returns them. */
const at = (id: string, iso: string, parent: string | null = null) => ({ id, createdAt: iso, parent });
const c = at("c", "2026-09-03T12:00:00Z", "b");
const b = at("b", "2026-09-02T12:00:00Z", "a");
const a = at("a", "2026-09-01T12:00:00Z", null);
const HISTORY = [c, b, a];

describe("envCurrent", () => {
  test("never restored: the newest record", () => {
    expect(envCurrent(HISTORY, { live: true, restoredTo: null, restoredAt: null }))
      .toEqual({ current: c, foreign: null });
  });

  test("restored and nothing pushed since: the restored record itself", () => {
    expect(envCurrent(HISTORY, { live: true, restoredTo: "a", restoredAt: "2026-09-03T18:00:00Z" }))
      .toEqual({ current: a, foreign: null });
  });

  test("restored then pushed: the new record on the restored branch, not the old tip", () => {
    const d = at("d", "2026-09-04T12:00:00Z", "a");
    expect(envCurrent([d, ...HISTORY], { live: true, restoredTo: "a", restoredAt: "2026-09-04T06:00:00Z" }))
      .toEqual({ current: d, foreign: null });
  });

  test("a newer record on a SIBLING branch is not current", () => {
    // Pushed after the restore, but descends from `b` — the branch the environment left.
    const sib = at("sib", "2026-09-04T12:00:00Z", "b");
    expect(envCurrent([sib, ...HISTORY], { live: true, restoredTo: "a", restoredAt: "2026-09-04T06:00:00Z" }))
      .toEqual({ current: a, foreign: null });
  });

  test("a foreign restored_to names no record: no current, and the id is reported", () => {
    expect(envCurrent(HISTORY, { live: true, restoredTo: "other-vol-snap", restoredAt: "2026-09-03T18:00:00Z" }))
      .toEqual({ current: null, foreign: "other-vol-snap" });
  });

  test("archived: no live environment sits anywhere", () => {
    expect(envCurrent(HISTORY, { live: false, restoredTo: null, restoredAt: null }))
      .toEqual({ current: null, foreign: null });
  });

  test("no records at all", () => {
    expect(envCurrent([], { live: true, restoredTo: null, restoredAt: null }))
      .toEqual({ current: null, foreign: null });
  });

  test("a record with no timestamp never wins the after-the-restore test", () => {
    const undated = at("undated", null as unknown as string, "a") as { id: string; createdAt: string | null; parent: string | null };
    expect(envCurrent([undated, ...HISTORY], { live: true, restoredTo: "a", restoredAt: "2026-09-04T06:00:00Z" }))
      .toEqual({ current: a, foreign: null });
  });
});
