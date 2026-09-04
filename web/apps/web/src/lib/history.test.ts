import { expect, test } from "bun:test";
import { FLAT, deltaLabel, eventSummary, attentionTone } from "./history";

test("an unavailable series reads as a placeholder, never as zero movement", () => {
  expect(FLAT.available).toBe(false);
  expect(FLAT.series).toEqual([]);
  expect(deltaLabel(FLAT)).toBe("history unavailable");
});

test("a delta says its direction and window in the tile's own sub-line", () => {
  const s = { series: [], summary: { last: 63, delta: 8, min: 55, max: 63 }, available: true };
  expect(deltaLabel(s)).toBe("+8 in the last 7 days");
  expect(deltaLabel({ ...s, summary: { ...s.summary, delta: -2 } })).toBe("-2 in the last 7 days");
  expect(deltaLabel({ ...s, summary: { ...s.summary, delta: 0 } })).toBe("unchanged over 7 days");
});

test("an event renders one sentence from actor, phrase and subject", () => {
  expect(
    eventSummary({
      id: "1",
      ts: "",
      kind: "request.approved",
      actor: "karthik",
      owner: "acme",
      target: "Quota/acme",
      region: null,
      attrs: { note: "workspaces 20 → 40" },
    }),
  ).toBe("karthik approved a request for acme");
  // A kind the console has no phrasing for still says who did what to which object, because a
  // history row is the record and dropping it would hide exactly the surprising events.
  expect(
    eventSummary({
      id: "2",
      ts: "",
      kind: "volume.reheal",
      actor: "system",
      owner: null,
      target: "Volume/vol-9f2a",
      region: "westeurope-k3s",
      attrs: {},
    }),
  ).toBe("system volume.reheal Volume/vol-9f2a");
});

test("attention rows are toned by kind, and an unknown kind is not silently calm", () => {
  expect(attentionTone("signal.firing")).toBe("critical");
  expect(attentionTone("over_quota")).toBe("warn");
  expect(attentionTone("draining")).toBe("info");
  expect(attentionTone("something_new")).toBe("warn");
});
