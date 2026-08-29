import { describe, expect, test } from "bun:test";
import { pendingPush } from "./pending-push";

describe("pendingPush", () => {
  const asked = { request: "r1", had: 3 };

  test("waits while the history has not grown", () => {
    expect(pendingPush(asked, 3)).toBe(asked);
  });

  test("clears once the record lands", () => {
    expect(pendingPush(asked, 4)).toBeNull();
  });

  test("a landed push stays landed when the history later shrinks", () => {
    // Deleting a record after a push used to put the page back into "uploading…".
    expect(pendingPush(null, 2)).toBeNull();
  });
});
