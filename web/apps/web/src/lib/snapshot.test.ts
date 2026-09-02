import { describe, expect, test } from "bun:test";
import { snapshotTime } from "./snapshot";

// One row exactly as `commit_model_history_rows` builds it
// (`crates/workspaces/src/api.rs:2012-2033`): camelCase `createdAt`, an RFC3339 string from
// `jiff::Timestamp`'s Display, plus the `phase` the TS type does not declare and the hardcoded
// `region: ""` / `state: null`. Recorded here so a rename on either side fails loudly.
const ROW = {
  id: "snap-4f2c",
  state: null,
  lineage: [],
  region: "",
  message: "before the migration",
  createdAt: "2026-09-02T11:04:07Z",
  parent: null,
  phase: "Ready",
};

describe("snapshotTime", () => {
  test("reads the wire's camelCase field", () => {
    expect(snapshotTime(ROW)).toBe(Date.parse("2026-09-02T11:04:07Z"));
    expect(Number.isFinite(snapshotTime(ROW))).toBe(true);
  });

  test("a row that never got a creation timestamp is NaN, not the epoch", () => {
    // `creation_timestamp()` is an Option (api.rs:2027). NaN renders as "Invalid Date", which is
    // honest; 1970 would be a lie the tree would happily sort on.
    expect(snapshotTime({ ...ROW, createdAt: null })).toBeNaN();
  });

  test("the old snake_case name is not what the server sends", () => {
    // The bug this file exists for: reading `created_at` off this row gave `undefined`, and
    // `new Date(undefined)` is Invalid Date on every snapshot row in the app.
    expect((ROW as Record<string, unknown>).created_at).toBeUndefined();
  });
});
