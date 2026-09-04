import { describe, expect, test } from "bun:test";
import { auditQueryString } from "./audit";

describe("auditQueryString", () => {
  test("encodes only the fields that are set, in a stable order", () => {
    expect(auditQueryString({ actor: "op@x.com", action: "" })).toBe("?actor=op%40x.com");
  });
  test("empty filter is an empty string", () => {
    expect(auditQueryString({})).toBe("");
  });
  test("keeps field order regardless of insertion order", () => {
    expect(auditQueryString({ to: "2026-09", actor: "a", limit: 50 })).toBe("?actor=a&to=2026-09&limit=50");
  });
});
