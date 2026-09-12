import { expect, mock, test } from "bun:test";

// `docs` is `server-only`; that guard throws outside a server component.
mock.module("server-only", () => ({}));
const { page } = await import("./docs");

test("a slug that is not a slug is a 404, never a file read", async () => {
  // Traversal, in both the shapes that reach a route: decoded by Next, and left encoded.
  expect(await page(["..", "..", "CLAUDE"])).toBeNull();
  expect(await page(["%2e%2e", "%2e%2e", "CLAUDE"])).toBeNull();
  expect(await page(["quick-start", "..", "..", "..", "package"])).toBeNull();
});
