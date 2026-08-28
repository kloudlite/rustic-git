import { expect, test } from "bun:test";
import { loginDestination, safeNext } from "./destination";

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

test("next is honoured once there is a token", () => {
  expect(
    loginDestination({ hasSession: true, hasToken: true, username: "ann", next: "/cli/authorize?code=AB-CD" }),
  ).toBe("/cli/authorize?code=AB-CD");
});

test("from=expired still wins over next", () => {
  expect(
    loginDestination({ hasSession: true, hasToken: true, username: "ann", from: "expired", next: "/cli/authorize" }),
  ).toBeNull();
});

test("no handle goes to welcome even with a next", () => {
  expect(loginDestination({ hasSession: true, hasToken: true, next: "/cli/authorize" })).toBe("/welcome");
});

test("an off-site next is refused, not followed", () => {
  for (const next of ["//evil.com", "https://evil.com", "http://evil.com/x", "/\\evil.com", "evil.com", ""]) {
    expect(loginDestination({ hasSession: true, hasToken: true, username: "ann", next })).toBe("/");
    expect(safeNext(next)).toBeUndefined();
  }
});

test("safeNext keeps a relative path whole", () => {
  expect(safeNext("/cli/authorize?code=AB-CD")).toBe("/cli/authorize?code=AB-CD");
});
