import { describe, expect, test } from "bun:test";
import { SUPERADMIN_AREAS, activeArea } from "./superadmin-nav";

describe("activeArea", () => {
  test("matches the exact area", () => {
    expect(activeArea("/superadmin/requests")).toBe("/superadmin/requests");
  });
  test("matches a detail page under an area by longest prefix", () => {
    expect(activeArea("/superadmin/owners/acme")).toBe("/superadmin/owners");
  });
  test("matches Overview only at the root, never as a prefix of every other area", () => {
    expect(activeArea("/superadmin/audit")).toBe("/superadmin/audit");
    expect(activeArea("/superadmin")).toBe("/superadmin");
  });
});

test("every area has a unique href", () => {
  expect(new Set(SUPERADMIN_AREAS.map((a) => a.href)).size).toBe(SUPERADMIN_AREAS.length);
});
