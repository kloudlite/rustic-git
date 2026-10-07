import { expect, test } from "bun:test";
import { menuItems } from "./slash.ts";

const ctx = {
  models: [] as { provider: string; id: string; hint: string }[],
  themes: [],
  logins: [
    { provider: "anthropic", type: "oauth", label: "Anthropic (Claude Pro/Max)" },
    { provider: "deepseek", type: "api_key", label: "DeepSeek API key" },
  ],
  settings: [],
  sessions: [],
};

// The models list is filtered to connected providers, so a first run has
// nothing to pick — an empty menu would be the only clue that /login exists.
test("/model offers the logins when no provider is connected", () => {
  const items = menuItems("/model ", ctx);
  expect(items.length).toBe(2);
  expect(items.every((i) => i.insert.startsWith("/login "))).toBe(true);
  expect(items[0]!.hint).toContain("connect");
});

test("/model lists models once a provider is connected", () => {
  const items = menuItems("/model ", {
    ...ctx,
    models: [{ provider: "deepseek", id: "deepseek-flash", hint: "DeepSeek V4.1 Flash" }],
  });
  expect(items.map((i) => i.insert)).toEqual(["/model deepseek/deepseek-flash"]);
});

// the login fallback still has to answer the filter the user is typing
test("the login fallback filters", () => {
  expect(menuItems("/model deep", ctx).map((i) => i.insert)).toEqual([
    "/login deepseek api_key",
  ]);
});

// Every key/value pair at once ran to 20-odd rows and buried everything that
// was not thinkingLevel, so /settings lists its keys and drills into one.
const settingsCtx = {
  ...ctx,
  settings: [
    { key: "thinking", value: "show", hint: "current" },
    { key: "thinking", value: "hide", hint: "" },
    { key: "thinkingLevel", value: "off", hint: "no reasoning" },
    { key: "thinkingLevel", value: "high", hint: "current · ~32k tokens" },
  ],
};

test("/settings lists its keys, with the current value as the hint", () => {
  expect(menuItems("/settings ", settingsCtx)).toEqual([
    { insert: "/settings thinking ", label: "thinking", hint: "show" },
    { insert: "/settings thinkingLevel ", label: "thinkingLevel", hint: "high" },
  ]);
});

// "thinking" is a prefix of "thinkingLevel"; the trailing space separates them
test("naming a setting lists only its values", () => {
  expect(menuItems("/settings thinkingLevel ", settingsCtx).map((i) => i.insert)).toEqual([
    "/settings thinkingLevel off",
    "/settings thinkingLevel high",
  ]);
  expect(menuItems("/settings thinking ", settingsCtx).map((i) => i.insert)).toEqual([
    "/settings thinking show",
    "/settings thinking hide",
  ]);
});
