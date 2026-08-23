import { expect, test } from "bun:test";
import { loginDestination } from "./destination";

test("signed out stays on the form", () => {
  expect(loginDestination({ hasSession: false, hasToken: false })).toBeNull();
});

test("signed in with a token goes home", () => {
  expect(loginDestination({ hasSession: true, hasToken: true, username: "ann" })).toBe("/");
});

test("no handle goes to welcome", () => {
  expect(loginDestination({ hasSession: true, hasToken: true })).toBe("/welcome");
});

test("signed in but tokenless stays, instead of bouncing off / forever", () => {
  expect(loginDestination({ hasSession: true, hasToken: false, username: "ann" })).toBeNull();
});

test("from=expired stays even with a token", () => {
  expect(
    loginDestination({ hasSession: true, hasToken: true, username: "ann", from: "expired" }),
  ).toBeNull();
});
