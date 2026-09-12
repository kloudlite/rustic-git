// Review finding #72 (2026-09-12): the shared-password door must be absent unless it was
// opened on purpose. Asserted on the provider LIST, because "registered and always refusing"
// would still publish /api/auth/callback/credentials.
import { afterEach, beforeEach, expect, mock, test } from "bun:test";

mock.module("server-only", () => ({}));

process.env.AUTH_SECRET = "test-secret";
process.env.AUTH_URL = "http://localhost:3000";

const { providers } = await import("./auth");

// Auth.js's Credentials() stamps every instance id "credentials"/name "Credentials" and keeps
// what was passed in under `options`, so that is where the provider under test is named.
const ids = () =>
  providers().map((p) => (p as { options?: { id?: string }; id?: string }).options?.id ?? (p as { id: string }).id);

beforeEach(() => {
  process.env.AUTH_ALLOWED_EMAILS = "ada@example.com";
  process.env.AUTH_SHARED_PASSWORD = "hunter2";
  process.env.AUTH_SHARED_PASSWORD_ENABLED = "1";
});

afterEach(() => {
  delete process.env.AUTH_ALLOWED_EMAILS;
  delete process.env.AUTH_SHARED_PASSWORD;
  delete process.env.AUTH_SHARED_PASSWORD_ENABLED;
});

test("present when both the flag and the password are set", () => {
  expect(ids()).toContain("credentials");
});

test("absent when the flag is off", () => {
  delete process.env.AUTH_SHARED_PASSWORD_ENABLED;
  expect(ids()).not.toContain("credentials");
  process.env.AUTH_SHARED_PASSWORD_ENABLED = "0";
  expect(ids()).not.toContain("credentials");
});

test("absent when the password is empty", () => {
  process.env.AUTH_SHARED_PASSWORD = "";
  expect(ids()).not.toContain("credentials");
});

test("the other providers are unaffected", () => {
  delete process.env.AUTH_SHARED_PASSWORD_ENABLED;
  expect(ids()).toContain("passkey");
  expect(ids()).toContain("email-link");
});
