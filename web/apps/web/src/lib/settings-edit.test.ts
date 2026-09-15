import { expect, test } from "bun:test";
import { canSave, changedFields, historyOf, saveBody, writeError } from "./settings";

const rows = [
  { name: "syncSecs", unit: "seconds", env: null, default: 300 },
  { name: "nixpkgs", unit: "string", env: null, default: "abc" },
  { name: "memberRemovalDeletes", unit: "bool", env: null, default: false },
];

test("only moved fields are sent, beside the note", () => {
  const stored = { syncSecs: 300, nixpkgs: "abc" };
  const changes = changedFields(rows, stored, { syncSecs: "600", nixpkgs: "abc", memberRemovalDeletes: false });
  expect(changes).toEqual({ syncSecs: 600 });
  expect(saveBody(changes, "  slower sync  ")).toEqual({ syncSecs: 600, note: "slower sync" });
});

test("a toggled bool and an emptied box", () => {
  expect(changedFields(rows, { syncSecs: 300 }, { syncSecs: "", memberRemovalDeletes: true })).toEqual({
    memberRemovalDeletes: true,
  });
});

test("unparsable number text reaches the api so its 422 names the field", () => {
  expect(changedFields(rows, {}, { syncSecs: "ten" })).toEqual({ syncSecs: "ten" });
});

test("save needs a change and a note", () => {
  expect(canSave({ syncSecs: 600 }, "")).toBe(false);
  expect(canSave({ syncSecs: 600 }, "   ")).toBe(false);
  expect(canSave({}, "why")).toBe(false);
  expect(canSave({ syncSecs: 600 }, "why")).toBe(true);
});

test("a 422 is shown verbatim, a 409 in words", () => {
  const msg = "syncSecs must be between 10 and 3600, got 5";
  expect(writeError({ kind: "invalid", message: msg })).toBe(msg);
  expect(writeError({ kind: "conflict", message: '{"name":"kloudlite-agent","ready":1,"desired":3}' })).toBe(
    "kloudlite-agent is still rolling out (1/3 ready); try again shortly",
  );
});

test("history from either scope's shape", () => {
  expect(historyOf({ history: [{ maxBody: 1 }] })).toEqual([{ maxBody: 1 }]);
  expect(historyOf({ metadata: { annotations: { "kloudlite.io/settings-history": '[{"syncSecs":60}]' } } })).toEqual([
    { syncSecs: 60 },
  ]);
  expect(historyOf({})).toEqual([]);
});
