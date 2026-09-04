import { expect, test } from "bun:test";
import { isRefusal, resultPill } from "./audit-result";
import type { AuditEntry } from "./audit";

const e = (result: string): AuditEntry => ({
  ts: "2026-09-04T14:41:08Z",
  actor: "karthik",
  action: "quota.set",
  target: "Quota/acme",
  reason: "",
  result,
});

test("a refusal reads as a first-class event with its own status", () => {
  expect(resultPill(e("ok"))).toEqual({ tone: "ok", label: "ok" });
  // A refused create IS the interesting row on this page — 409 must not render as "ok" or as
  // an absence.
  expect(resultPill(e("error: 409"))).toEqual({ tone: "warn", label: "error: 409" });
  expect(resultPill(e("error: 403"))).toEqual({ tone: "critical", label: "error: 403" });
  expect(resultPill(e("error: 500"))).toEqual({ tone: "critical", label: "error: 500" });
});

test("an empty result is ok, not a blank pill", () => {
  expect(resultPill(e(""))).toEqual({ tone: "ok", label: "ok" });
});

test("isRefusal is the page's one refusal predicate", () => {
  expect([e("ok"), e("error: 409"), e("error: 500")].filter(isRefusal).length).toBe(2);
});
