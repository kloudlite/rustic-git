import { expect, test } from "bun:test";
import { signAssertion, verifyAssertion } from "./assertion";

process.env.AUTH_SECRET = "test-secret";

test("an email with dots round-trips", () => {
  const email = "first.last@example.co.uk";
  expect(verifyAssertion(signAssertion(email))).toBe(email);
});

test("the email is lowercased on the way in", () => {
  expect(verifyAssertion(signAssertion("Ada@Example.com"))).toBe("ada@example.com");
});

test("an expired assertion is refused", () => {
  const stale = signAssertion("ada@example.com", Date.now() - 120_000);
  expect(verifyAssertion(stale)).toBeNull();
});

test("a tampered email is refused", () => {
  const a = signAssertion("ada@example.com");
  const swapped = `eve@example.com${a.slice("ada@example.com".length)}`;
  expect(verifyAssertion(swapped)).toBeNull();
});

test("a tampered mac is refused", () => {
  const a = signAssertion("ada@example.com");
  expect(verifyAssertion(a.slice(0, -1) + (a.endsWith("A") ? "B" : "A"))).toBeNull();
});

test("garbage is refused", () => {
  expect(verifyAssertion("")).toBeNull();
  expect(verifyAssertion("nodots")).toBeNull();
  expect(verifyAssertion("one.dot")).toBeNull();
});
