//! `/btw`: a side question answered from the conversation so far, never recorded.
//!
//! Same pattern as pi's own `summarizeForBugReport`: the conversation is serialized into ONE plain
//! user message, so there are no tool_use blocks to replay, no tool definitions to send and no
//! dangling-call problem, whichever engine owns the session and whether or not a turn is running.
//! Nothing here touches the session: it reads `messages`, makes a separate one-shot request and
//! returns the text. Callers must not record it (transcript, session file, consent words).
import { convertToLlm, estimateTokens, serializeConversation } from "@earendil-works/pi-coding-agent";
import { streamSimple } from "@earendil-works/pi-ai/compat";

export const BTW_SYSTEM = "You answer a side question about a coding conversation. Be brief and exact. Use markdown.";

const INSTRUCTION =
  "This is a side question from the person. It is not part of the task. Answer it briefly from the conversation above. You have no tools. Do not offer to do work.";

/** Newest messages that fit the budget (copied from pi's bug-report.js; it is not exported). */
function selectMessages(messages: any[], budget: number): any[] {
  const selected: any[] = [];
  let tokens = 0;
  for (let i = messages.length - 1; i >= 0; i--) {
    const next = estimateTokens(messages[i]);
    if (selected.length > 0 && tokens + next > budget) break;
    selected.push(messages[i]);
    tokens += next;
  }
  return selected.reverse();
}

export function btwPrompt(messages: any[], question: string, contextWindow: number): string {
  const selected = selectMessages(messages, Math.floor((contextWindow > 0 ? contextWindow : 128_000) * 0.6));
  return [
    selected.length < messages.length ? `Note: only the last ${selected.length} of ${messages.length} messages are shown.` : undefined,
    `<conversation>\n${serializeConversation(convertToLlm(selected))}\n</conversation>`,
    `<question>\n${question}\n</question>`,
    INSTRUCTION,
  ]
    .filter((p) => p !== undefined)
    .join("\n\n");
}

/** `session` is a pi AgentSession; `_getSummarizationRequestAuth` is private in the types only. */
export async function piBtw(session: any, question: string): Promise<string> {
  if (!session.model) throw new Error("No model selected");
  const auth = await session._getSummarizationRequestAuth(session.model, undefined);
  const { model } = auth;
  const context = {
    systemPrompt: BTW_SYSTEM,
    messages: [{ role: "user", content: [{ type: "text", text: btwPrompt(session.messages, question, model.contextWindow) }], timestamp: Date.now() }],
  };
  const options = {
    apiKey: auth.apiKey,
    headers: auth.headers,
    env: auth.env,
    maxTokens: Math.min(4096, model.maxTokens > 0 ? model.maxTokens : Infinity),
    cacheRetention: "none",
  };
  const stream = session.agent.streamFunction ?? streamSimple;
  const res = await (await stream(model, context, options)).result();
  if (res.stopReason === "error") throw new Error(res.errorMessage ?? "request failed");
  const text = res.content.filter((c: any) => c.type === "text").map((c: any) => c.text).join("").trim();
  return text || "(no answer)";
}
