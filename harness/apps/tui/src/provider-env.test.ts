import { expect, test } from "bun:test";
import { providerAuth } from "@kloudlite-tui/agent";

// the names come from pi itself (a recording ctx.env), not from a
// `${ID}_API_KEY` guess, which was wrong for 13 of 40 providers
test("provider env vars are the ones the provider really reads", async () => {
  const byId = new Map((await providerAuth()).map((p) => [p.provider, p.envKeys]));

  // the guess would have said HUGGINGFACE_API_KEY / GITHUB_COPILOT_API_KEY
  expect(byId.get("huggingface")).toEqual(["HF_TOKEN"]);
  expect(byId.get("github-copilot")).toEqual(["COPILOT_GITHUB_TOKEN"]);
  expect(byId.get("vercel-ai-gateway")).toEqual(["AI_GATEWAY_API_KEY"]);
  // bedrock takes AWS credentials, not an API key of its own
  expect(byId.get("amazon-bedrock")).toContain("AWS_ACCESS_KEY_ID");
  // the convention still holds where it holds
  expect(byId.get("deepseek")).toEqual(["DEEPSEEK_API_KEY"]);
  // Claude signs in through Claude Code, so no env var is offered for it
  expect(byId.get("anthropic")).toEqual([]);
  // OAuth-only: no hint to show
  expect(byId.get("openai-codex")).toEqual([]);
});
