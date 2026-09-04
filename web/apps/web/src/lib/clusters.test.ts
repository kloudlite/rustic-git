import { describe, expect, test } from "bun:test";
import { isDrained, parseDecommissionStatus, settingsStatusTone } from "./clusters";

describe("parseDecommissionStatus", () => {
  test("absent is none", () => {
    expect(parseDecommissionStatus(null)).toEqual({ kind: "none" });
    expect(parseDecommissionStatus(undefined)).toEqual({ kind: "none" });
    expect(parseDecommissionStatus("")).toEqual({ kind: "none" });
  });

  test("draining counters", () => {
    expect(parseDecommissionStatus("draining running=2 owned=5 copies=4 thin=1")).toEqual({
      kind: "draining", running: 2, owned: 5, copies: 4, thin: 1,
    });
  });

  test("drained timestamp", () => {
    expect(parseDecommissionStatus("drained 2026-09-04T10:00:00Z")).toEqual({
      kind: "drained", at: "2026-09-04T10:00:00Z",
    });
  });

  test("unrecognized shape still reads as draining, not none", () => {
    expect(parseDecommissionStatus("something else")).toEqual({ kind: "draining", running: 0, owned: 0, copies: 0, thin: 0 });
  });
});

describe("isDrained", () => {
  test("true only once drained", () => {
    expect(isDrained("drained 2026-09-04T10:00:00Z")).toBe(true);
    expect(isDrained("draining running=0 owned=0 copies=0 thin=0")).toBe(false);
    expect(isDrained(null)).toBe(false);
  });
});

describe("settingsStatusTone", () => {
  test("known values pass through, unknown is neutral", () => {
    expect(settingsStatusTone("present")).toBe("present");
    expect(settingsStatusTone("absent")).toBe("absent");
    expect(settingsStatusTone("stale")).toBe("stale");
    expect(settingsStatusTone("parse-error")).toBe("unknown");
  });
});
