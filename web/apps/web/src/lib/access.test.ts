import { describe, expect, test } from "bun:test";
import { removeDisabledReason } from "./access";
import type { SuperAdmin } from "./api";

function admin(id: string): SuperAdmin {
  return { _id: id, addedAt: "2026-01-01T00:00:00Z", addedBy: "bootstrap" };
}

describe("removeDisabledReason", () => {
  test("refuses your own row, case- and whitespace-insensitive", () => {
    const rows = [admin("a@x.com"), admin("b@x.com")];
    expect(removeDisabledReason(rows[0], rows, " A@X.com ")).toBe("You cannot remove your own administrator claim");
  });

  test("refuses the last row even when it isn't yours", () => {
    const rows = [admin("a@x.com")];
    expect(removeDisabledReason(rows[0], rows, "op@x.com")).toBe("The last administrator cannot be removed");
  });

  test("allows removal of a normal row", () => {
    const rows = [admin("a@x.com"), admin("b@x.com")];
    expect(removeDisabledReason(rows[1], rows, "a@x.com")).toBeNull();
  });
});
