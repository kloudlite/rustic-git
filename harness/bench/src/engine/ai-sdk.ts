// The Llm the terminal program hands to the library, on the Vercel AI SDK. Only the program imports this file.
import { generateText, jsonSchema, tool, type LanguageModel, type ModelMessage } from "ai";
import { createAnthropic } from "@ai-sdk/anthropic";
import { createDeepSeek } from "@ai-sdk/deepseek";
import { createOpenAICompatible } from "@ai-sdk/openai-compatible";
import type { Llm, LlmMessage, LlmTool } from "./executor.ts";
import { trackUsage } from "./usage.ts";

export const LLM_TIMEOUT_MS = 180_000;
const MAX_STEPS = 40; // a session that never submits stops here

// JEVHARN_MODEL is provider/id. The key is the provider's usual env var; any other provider is reached as
// OpenAI-compatible at JEVHARN_BASE_URL with JEVHARN_API_KEY.
function modelFromEnv(): { provider: string; id: string; model: LanguageModel } {
  const spec = process.env.JEVHARN_MODEL ?? "deepseek/deepseek-v4-pro";
  const cut = spec.indexOf("/"), provider = spec.slice(0, cut), id = spec.slice(cut + 1);
  if (provider === "anthropic") return { provider, id, model: createAnthropic()(id) };
  if (provider === "deepseek") return { provider, id, model: createDeepSeek()(id) };
  const baseURL = process.env.JEVHARN_BASE_URL;
  if (!baseURL) throw new Error(`JEVHARN_MODEL names provider "${provider}": set JEVHARN_BASE_URL (and JEVHARN_API_KEY) to reach it as OpenAI-compatible`);
  return { provider, id, model: createOpenAICompatible({ name: provider, baseURL, apiKey: process.env.JEVHARN_API_KEY })(id) };
}

// A cache marker is a paid write, so it goes only where the text comes back as a prefix. Providers that cache by
// prefix on their own (DeepSeek, OpenAI) ignore it.
const MARK = { anthropic: { cacheControl: { type: "ephemeral" } } } as const;

// One fresh session per call. The request is ordered stable first: tools, system, then the per-call text.
export const makeAiSdkLlm = (pick: () => { provider: string; id: string; model: LanguageModel } = modelFromEnv): Llm => async (_cwd, system, tools) => {
  const { provider, id, model } = pick();
  const kind = tools.at(-1)?.name.replace("submit_", "") ?? "oneshot";
  let terminated = false;
  const sdkTools = Object.fromEntries(tools.map((t: LlmTool) => [t.name, tool({
    description: t.description,
    inputSchema: jsonSchema(t.parameters as never),
    execute: async (input: unknown, o: { toolCallId: string }) => {
      const r = await t.execute(o.toolCallId, input);
      if (r.terminate) terminated = true;
      return r.content.map((c) => c.text).join("\n");
    },
  })]));
  const history: ModelMessage[] = [];
  const messages: LlmMessage[] = [];
  return {
    model: { provider, id },
    messages,
    prompt: async (text) => {
      history.push({ role: "user", content: text });
      try {
        const r = await generateText({
          model, tools: sdkTools,
          // system and tools are the same bytes on every call of this kind: the one marker that always pays
          instructions: { role: "system", content: system, providerOptions: MARK },
          messages: history,
          stopWhen: ({ steps }) => terminated || steps.length >= MAX_STEPS,
          // Within one session each turn re-sends the turns before it, so from the second turn on the last
          // message is marked. The first turn's task text is never a prefix of another session: no marker.
          prepareStep: ({ stepNumber, messages: m }) => stepNumber === 0 ? undefined
            : { messages: m.map((x, i) => i === m.length - 1 ? { ...x, providerOptions: MARK } : x) },
          // ponytail: one fixed limit for every kind of call; per-kind limits if a long plan proves too slow for it.
          abortSignal: AbortSignal.timeout(LLM_TIMEOUT_MS),
        });
        for (const s of r.steps) trackUsage(kind, s.usage.inputTokens, s.usage.outputTokens, s.usage.inputTokenDetails?.cacheReadTokens);
        history.push(...r.response.messages);
        messages.push({ role: "assistant", stopReason: r.finishReason, content: [{ type: "text", text: r.text }] });
      } catch (e) {
        if ((e as Error).name === "TimeoutError") throw new Error(`the ${kind} LLM call gave no answer in ${LLM_TIMEOUT_MS / 1000}s`);
        messages.push({ role: "assistant", stopReason: "error", errorMessage: (e as Error).message, content: [] });
      }
    },
  };
};
