import { describe, expect, test } from "bun:test";
import { basedOnSentence, cloneResult, noticesFor } from "./ws-status";

describe("noticesFor", () => {
  test("a stopped workspace still copying says so, and says when it will be safe", () => {
    const n = noticesFor({
      state: "stopped",
      replicated: { ready: false, reason: "AwaitingReplica", message: "no other node holds the final sync point yet" },
    });
    expect(n).toEqual([{ tone: "info", text: "Still copying to another node — it can only start on its current node until that finishes." }]);
  });

  test("replicated says it is safe to start anywhere", () => {
    const n = noticesFor({
      state: "stopped",
      replicated: { ready: true, reason: "Replicated", message: "another node holds the final sync point" },
    });
    expect(n).toEqual([{ tone: "info", text: "Copied to another node — safe to start anywhere." }]);
  });

  test("replicas: 1 says why it will never finish copying", () => {
    const n = noticesFor({
      state: "stopped",
      replicated: { ready: false, reason: "AwaitingReplica", message: "no replica is configured for this volume" },
    });
    expect(n[0].text).toBe("No replica is configured, so this can only ever start on its current node.");
  });

  test("an interrupted workspace is a warning, and offers the clone rather than a start", () => {
    const n = noticesFor({ state: "ready", degraded: { ready: true, reason: "NodeDead", message: "node n1 is down" } });
    expect(n).toEqual([{
      tone: "warning",
      text: "Its node is down. It resumes when the node returns — or clone it from the last synced point.",
    }]);
  });

  test("a node being retired is stated once, without alarm", () => {
    const n = noticesFor({
      state: "ready",
      decommissioning: { ready: true, reason: "NodeLeaving", message: "this node is being retired" },
    });
    expect(n).toEqual([{ tone: "info", text: "This node is being retired; stop when convenient and the next start lands elsewhere." }]);
  });

  // A condition that has flipped back to False keeps its reason; reading the reason alone would
  // leave "its node is down" on the page forever after the node came back.
  test("a cleared Degraded says nothing, even with the reason still NodeDead", () => {
    expect(noticesFor({ state: "ready", degraded: { ready: false, reason: "NodeDead", message: "node n1 is down" } })).toEqual([]);
  });

  test("a cleared Decommissioning says nothing, even with the reason still NodeLeaving", () => {
    expect(noticesFor({ state: "ready", decommissioning: { ready: false, reason: "NodeLeaving", message: "retired" } })).toEqual([]);
  });

  test("a running workspace with nothing to say says nothing", () => {
    expect(noticesFor({ state: "ready" })).toEqual([]);
  });
});

/** A pinned zone, so the sentence's shape is asserted without pinning the machine's. */
const utc = new Intl.DateTimeFormat("en-GB", { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false, timeZone: "UTC" });
/** The default formatter, mirrored: the reader's own zone, whatever CI happens to run in. */
const local = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });

describe("basedOnSentence", () => {
  test("an ordinary clone names the cut it was made from", () => {
    expect(basedOnSentence({ snapshot: "clone-ws-1-cafe", at: null, age_seconds: 0, interrupted: false }))
      .toBe("Cloned from a sync point taken just now.");
  });

  test("an interrupted clone states the gap, because that is the whole decision", () => {
    expect(basedOnSentence({ snapshot: "sync-ws-1-bbbb", at: "2026-09-03T14:32:07Z", age_seconds: 360, interrupted: true }, utc))
      .toBe("Cloned from the sync point of 14:32:07, 6 minutes before the node went down.");
  });
});

describe("cloneResult", () => {
  test("carries the sentence when the clone says what it was based on", () => {
    const r = cloneResult({ based_on: { snapshot: "sync-ws-1-bbbb", at: "2026-09-03T14:32:07Z", age_seconds: 360, interrupted: true } });
    // The time is rendered in the READER's zone, so this asserts through the same formatter rather
    // than pinning a string only a UTC machine would produce.
    expect(r.basedOn).toBe(`Cloned from the sync point of ${local.format(new Date("2026-09-03T14:32:07Z"))}, 6 minutes before the node went down.`);
  });

  test("an environment clone, which carries no based_on, is a plain success", () => {
    expect(cloneResult({})).toEqual({ ok: true, basedOn: undefined });
  });
});
