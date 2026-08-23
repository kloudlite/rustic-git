import { expect, test } from "bun:test";
import { pathHref } from "./utils";

test("a space and a hash in a filename are escaped, the slashes are not", () => {
  expect(pathHref("a b/c#d.md")).toBe("a%20b/c%23d.md");
});

test("empty segments are dropped, so a path does not round-trip verbatim", () => {
  // Matches `filePath` in lib/browse.ts, which normalises the same way for api
  // calls -- the two must agree or a link points at a path the server rejects.
  expect(pathHref("a//b")).toBe("a/b");
  expect(pathHref("/a/b/")).toBe("a/b");
});

test("a question mark stays in the path instead of starting a query", () => {
  expect(pathHref("is it?.txt")).toBe("is%20it%3F.txt");
});
