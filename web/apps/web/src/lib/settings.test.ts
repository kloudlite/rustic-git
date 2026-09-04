import { expect, test } from "bun:test";
import { conflictMessage, rolloutStateLabel, settled } from "@/lib/settings";

test("conflictMessage turns a workloads::conflict body into a plain sentence", () => {
  expect(conflictMessage('{"name":"rustic-git-worker","ready":2,"desired":3}')).toBe(
    "rustic-git-worker is still rolling out (2/3 ready); try again shortly",
  );
});

test("conflictMessage falls back to the raw text when the body isn't the expected shape", () => {
  expect(conflictMessage("not json")).toBe("not json");
});

test("rolloutStateLabel shows the ready/desired count only while rolling out", () => {
  expect(rolloutStateLabel("Stable", 3, 3)).toBe("Stable");
  expect(rolloutStateLabel("RollingOut", 2, 3)).toBe("Rolling out (2/3 ready)");
});

test("settled is true once ready catches up to desired", () => {
  expect(settled({ ready: 2, desired: 3 })).toBe(false);
  expect(settled({ ready: 3, desired: 3 })).toBe(true);
  expect(settled({ ready: 4, desired: 3 })).toBe(true);
});
