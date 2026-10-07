import { expect, test } from "bun:test";
import { webFetch, webSearch, searchProvider } from "./web.ts";

test("web_fetch strips markup to readable text", async () => {
  const out = await webFetch.run({ url: "https://example.com" });
  expect(out).toContain("Example Domain");
  // script/style contents are not text and must not survive the strip
  expect(out).not.toContain("<");
});

test("web_fetch reports a bad URL instead of throwing", async () => {
  expect(await webFetch.run({ url: "ftp://nope" })).toContain("not an http(s) URL");
  expect(await webFetch.run({ url: "https://example.com/nothing-here-404" })).toContain("error");
});

test("web_fetch truncates rather than flooding the context", async () => {
  const out = await webFetch.run({ url: "https://example.com", maxChars: 40 });
  expect(out).toContain("truncated at 40 characters");
});

// Without a key the tool has to say so — an empty result reads as "nothing found"
test("web_search names the key it needs when none is set", async () => {
  if (searchProvider()) return;
  const out = await webSearch.run({ query: "anything" });
  expect(out).toContain("BRAVE_API_KEY");
});
