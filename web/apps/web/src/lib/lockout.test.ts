import { expect, test } from "bun:test";
import { Lockout } from "./lockout";

test("five failures inside a minute lock the key until the window passes", () => {
  const l = new Lockout(5, 60_000);
  const t0 = 1_000_000;
  for (let i = 0; i < 4; i++) l.fail("ada@example.com", t0 + i * 1000);
  expect(l.locked("ada@example.com", t0 + 5000)).toBe(false);
  l.fail("ada@example.com", t0 + 5000);
  expect(l.locked("ada@example.com", t0 + 6000)).toBe(true);
  expect(l.locked("bob@example.com", t0 + 6000)).toBe(false);
  expect(l.locked("ada@example.com", t0 + 5000 + 60_000)).toBe(false);
});

test("a success clears the count", () => {
  const l = new Lockout(2, 60_000);
  l.fail("k");
  l.clear("k");
  l.fail("k");
  expect(l.locked("k")).toBe(false);
});
