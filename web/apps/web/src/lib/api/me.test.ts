import { describe, expect, test } from "bun:test";
import { spaceTeam, spaceUses } from "./me";

describe("space environment", () => {
  const choices = [
    { team: "acme", environment: "env-1", region: "centralindia" },
    { team: "karthik", environment: "env-2", region: null },
  ];
  test("a space uses exactly the environment it chose, per team", () => {
    expect(spaceUses(choices, "acme", "env-1")).toBe(true);
    expect(spaceUses(choices, "Acme", "env-1")).toBe(true);
    expect(spaceUses(choices, "acme", "env-2")).toBe(false);
    expect(spaceUses(choices, "karthik", "env-2")).toBe(true);
    expect(spaceUses([], "acme", "env-1")).toBe(false);
  });
  test("the team is the owner, folded", () => {
    expect(spaceTeam("Acme")).toBe("acme");
  });
});
