import { expect, test } from "bun:test";
import { matchesSearch, takesEffect } from "./config-rows";
import type { SettingsSchemaRow } from "./api";

const row = (p: Partial<SettingsSchemaRow>): SettingsSchemaRow => ({
  name: "ws_sync_secs",
  description: "how often a sync point is cut",
  unit: "s",
  range: { min: 30, max: 3600 },
  default: 300,
  env: null,
  mark: "live",
  readers: ["rustic-git-agent"],
  ...p,
});

test("a live field says live; a boot field names the reader it rolls", () => {
  expect(takesEffect(row({}))).toBe("live");
  expect(takesEffect(row({ mark: "boot", readers: ["rustic-git-agent"] }))).toBe("boot: rolls rustic-git-agent");
  // More than one reader: name them all, because a save rolls all of them.
  expect(takesEffect(row({ mark: "boot", readers: ["rustic-git-srv", "rustic-git-worker"] }))).toBe(
    "boot: rolls rustic-git-srv, rustic-git-worker",
  );
  // A boot field with no reader still must not read as live.
  expect(takesEffect(row({ mark: "boot", readers: [] }))).toBe("boot");
});

test("the search box matches the field name and its description", () => {
  expect(matchesSearch(row({}), "sync")).toBe(true);
  expect(matchesSearch(row({}), "SYNC")).toBe(true);
  expect(matchesSearch(row({}), "how often")).toBe(true);
  expect(matchesSearch(row({}), "registry")).toBe(false);
  expect(matchesSearch(row({}), "")).toBe(true);
});
