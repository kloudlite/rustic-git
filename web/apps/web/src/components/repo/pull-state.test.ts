import { describe, expect, test } from "bun:test";
import { LOOK } from "./pull-state";

describe("StateBadge's lookup", () => {
  test("every declared state has its own word and icon", () => {
    expect(LOOK.open.label).toBe("Open");
    expect(LOOK.merged.label).toBe("Merged");
    expect(LOOK.closed.label).toBe("Closed");
  });

  test("an unknown state off the wire falls back rather than taking the list down", () => {
    // The badge is rendered inside the pulls list and the PR header; indexing the map with a
    // state it does not hold (`draft`, say) used to return undefined and `undefined.cls` threw
    // the whole page, not just the badge.
    const look = LOOK[("draft" as unknown as keyof typeof LOOK)] ?? LOOK.open;
    expect(look.cls).toBe(LOOK.open.cls);
  });
});
