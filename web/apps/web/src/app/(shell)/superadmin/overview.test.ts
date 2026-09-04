import { describe, expect, test } from "bun:test";
import { needsNothing } from "./overview";

describe("needsNothing", () => {
  test("true when both are empty", () => {
    expect(needsNothing({ pendingRequests: [], attention: [] })).toBe(true);
  });

  test("false with a pending request", () => {
    expect(needsNothing({ pendingRequests: [{} as never], attention: [] })).toBe(false);
  });

  test("false with an attention item", () => {
    expect(needsNothing({ pendingRequests: [], attention: [{} as never] })).toBe(false);
  });
});
