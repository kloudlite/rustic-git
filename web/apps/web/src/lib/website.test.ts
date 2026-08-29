import { expect, test } from "bun:test";
import { safeWebsite } from "./website";

test("http and https pass through untouched", () => {
  expect(safeWebsite("https://example.com")).toBe("https://example.com");
  expect(safeWebsite("http://example.com/a?b=c#d")).toBe("http://example.com/a?b=c#d");
});

test("every other scheme, and anything that is not a URL, is refused", () => {
  for (const bad of [
    "javascript:alert(1)",
    "data:text/html,hi",
    "vbscript:x",
    "file:///etc/passwd",
    "example.com",
    "//example.com",
    "https://",
    "",
    undefined,
  ]) {
    expect(safeWebsite(bad)).toBeUndefined();
  }
});
