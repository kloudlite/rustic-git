import { expect, test } from "bun:test";
import {
  bootReaders, changedFields, conflictMessage, confirmationFor, mergeRows, pendingKeys, type SettingRow,
} from "@/lib/settings";

const row = (over: Partial<SettingRow>): SettingRow => ({
  key: "syncSecs",
  description: "",
  unit: "seconds",
  value: 60,
  envValue: null,
  defaultValue: 60,
  range: { min: 10, max: 3600 },
  mark: "live",
  readers: [],
  ...over,
});

test("changedFields only sends rows the edit actually touched", () => {
  const rows = [row({ key: "a", value: 1 }), row({ key: "b", value: 2 })];
  const patch = changedFields(rows, { a: 1, b: 5, c: 9 });
  expect(patch).toEqual({ b: 5 });
});

test("changedFields is empty when nothing was edited", () => {
  const rows = [row({ key: "a", value: 1 })];
  expect(changedFields(rows, { a: 1 })).toEqual({});
});

test("bootReaders unions and dedupes readers across every changed boot row, skips live rows", () => {
  const rows = [
    row({ key: "a", mark: "boot", readers: ["rustic-git-worker"] }),
    row({ key: "b", mark: "boot", readers: ["rustic-git-worker", "rustic-git-gateway"] }),
    row({ key: "c", mark: "live", readers: ["should-not-appear"] }),
  ];
  expect(bootReaders(rows, ["a", "b", "c"])).toEqual(["rustic-git-gateway", "rustic-git-worker"]);
});

test("a live-only diff needs no confirmation", () => {
  const rows = [row({ key: "a", mark: "live" })];
  expect(confirmationFor(rows, ["a"])).toEqual({ needsConfirm: false });
});

test("a boot-marked diff names the roll and needs one confirmation", () => {
  const rows = [row({ key: "a", mark: "boot", readers: ["rustic-git-worker"] })];
  const c = confirmationFor(rows, ["a"]);
  expect(c).toEqual({
    needsConfirm: true,
    message: "Save and roll: rustic-git-worker",
    readers: ["rustic-git-worker"],
    needsSecondConfirm: false,
  });
});

test("rustic-git-srv among the readers earns a second confirmation", () => {
  const rows = [row({ key: "sshHost", mark: "boot", readers: ["rustic-git-srv", "rustic-git-gateway"] })];
  const c = confirmationFor(rows, ["sshHost"]);
  expect(c.needsConfirm && c.needsSecondConfirm).toBe(true);
});

test("pendingKeys clears once the polled value matches what was saved", () => {
  expect(pendingKeys({ a: 5, b: 2 }, { a: 5, b: 9 })).toEqual(["b"]);
  expect(pendingKeys({ a: 5 }, { a: 5 })).toEqual([]);
});

test("conflictMessage turns a workloads::conflict body into a plain sentence", () => {
  expect(conflictMessage('{"name":"rustic-git-worker","ready":2,"desired":3}')).toBe(
    "rustic-git-worker is still rolling out (2/3 ready); try again shortly",
  );
});

test("conflictMessage falls back to the raw text when the body isn't the expected shape", () => {
  expect(conflictMessage("not json")).toBe("not json");
});

test("mergeRows resolves value from the stored document, falling back to the schema default", () => {
  const rows = mergeRows(
    [
      { name: "maxBody", description: "d", unit: "bytes", range: { min: 1, max: 9 }, mark: "live", readers: [], default: 100, env: "X" },
      { name: "sshPort", description: "d2", unit: "port", range: null, mark: "boot", readers: ["rustic-git-gateway"], default: 22, env: null },
    ],
    { maxBody: 500 },
    "alice",
    "2026-01-01T00:00:00Z",
  );
  expect(rows[0]).toMatchObject({ key: "maxBody", value: 500, defaultValue: 100, lastChangedBy: "alice" });
  expect(rows[1]).toMatchObject({ key: "sshPort", value: 22, mark: "boot", readers: ["rustic-git-gateway"] });
});
