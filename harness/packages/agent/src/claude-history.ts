/**
 * pi's transcript as Claude Code transcript entries.
 *
 * pi's session file is the one record of a conversation (spec
 * 2026-10-08-claude-tool-host-design.md, section 4). When a Claude query
 * starts - a switch from a pi model, a reopen, a dead child - Claude Code is
 * resumed from these entries through `Options.sessionStore.load`, so the model
 * sees every earlier turn whichever provider produced it.
 *
 * Entry shape verified by spike 2026-10-08 on claude-haiku-4-5. The format is
 * Claude Code's undocumented JSONL and `SessionStore` is `@alpha`: the SDK is
 * pinned exactly and the live smoke re-checks it on any bump.
 *
 * Dropped on purpose: thinking from any non-Anthropic api, whatever its
 * signature (pi-ai keeps an OpenAI reasoning item as JSON there, the API
 * rejects it), and redacted thinking; pi's `system`
 * messages (Claude gets pi's prompt as its system prompt instead).
 */
import { randomUUID } from "node:crypto";
import { convertToLlm } from "@earendil-works/pi-coding-agent";

export const TOOL_PREFIX = "mcp__kl__";
/** Claude Code version stamped on entries; load does not check it. */
const ENTRY_VERSION = "2.1.291";

export type ClaudeEntry = { type: string; uuid: string; [k: string]: unknown };

const blocks = (content: any) =>
  typeof content === "string"
    ? content
    : (content ?? []).flatMap((b: any) =>
        b.type === "text"
          ? [{ type: "text", text: b.text }]
          : b.type === "image"
            ? [{ type: "image", source: { type: "base64", media_type: b.mimeType, data: b.data } }]
            : [],
      );

// a lone text block goes as a plain string, the shape the spike verified
const userContent = (content: any) => {
  const b = blocks(content);
  return Array.isArray(b) && b.length === 1 && b[0].type === "text" ? b[0].text : b;
};

const stopReason = (r: string) => (r === "toolUse" ? "tool_use" : r === "length" ? "max_tokens" : "end_turn");

export function toClaudeEntries(messages: any[], o: { sessionId: string; cwd: string; model: string }): ClaudeEntry[] {
  const out: ClaudeEntry[] = [];
  let parent: string | null = null;
  // tool_use id -> uuid of the assistant entry that issued it, until answered
  const open = new Map<string, string>();
  const push = (type: string, message: unknown, ts: number | undefined, extra: object = {}) => {
    const uuid = randomUUID();
    out.push({
      parentUuid: parent,
      isSidechain: false,
      userType: "external",
      entrypoint: "sdk-cli",
      cwd: o.cwd,
      sessionId: o.sessionId,
      version: ENTRY_VERSION,
      gitBranch: "",
      type,
      message,
      uuid,
      timestamp: new Date(ts ?? Date.now()).toISOString(),
      ...extra,
    });
    parent = uuid;
    return uuid;
  };
  const result = (id: string, content: unknown, isError: boolean, ts?: number) => {
    push("user", { role: "user", content: [{ type: "tool_result", tool_use_id: id, content, is_error: isError }] }, ts, {
      sourceToolAssistantUUID: open.get(id),
    });
    open.delete(id);
  };
  // the API refuses a tool_use with no tool_result after it (an aborted turn)
  const closeOpen = () => {
    for (const id of [...open.keys()]) result(id, [{ type: "text", text: "Tool call was interrupted." }], true);
  };
  for (const m of convertToLlm(messages) as any[]) {
    if (m.role === "user") {
      closeOpen();
      push("user", { role: "user", content: userContent(m.content) }, m.timestamp, { promptId: randomUUID() });
    } else if (m.role === "assistant") {
      closeOpen();
      const anthropic = m.api === "anthropic-messages";
      const content = (m.content ?? []).flatMap((b: any) =>
        b.type === "text"
          ? [{ type: "text", text: b.text }]
          : b.type === "thinking" && anthropic && b.thinkingSignature && !b.redacted
            ? [{ type: "thinking", thinking: b.thinking, signature: b.thinkingSignature }]
            : b.type === "toolCall"
              ? [{ type: "tool_use", id: b.id, name: TOOL_PREFIX + b.name, input: b.arguments ?? {} }]
              : [],
      );
      if (!content.length) continue;
      const uuid = push(
        "assistant",
        {
          model: o.model,
          id: `msg_${randomUUID().replaceAll("-", "")}`,
          type: "message",
          role: "assistant",
          content,
          stop_reason: stopReason(m.stopReason),
          stop_sequence: null,
          usage: { input_tokens: m.usage?.input ?? 0, output_tokens: m.usage?.output ?? 0 },
        },
        m.timestamp,
      );
      for (const b of content) if (b.type === "tool_use") open.set(b.id, uuid);
    } else if (m.role === "toolResult" && open.has(m.toolCallId)) {
      result(m.toolCallId, blocks(m.content), !!m.isError, m.timestamp);
    }
  }
  closeOpen();
  return out;
}
