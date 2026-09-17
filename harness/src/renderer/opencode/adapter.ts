import type { AssistantMessage, Message as OcMessage, Part, ToolPart, ToolState, UserMessage } from "@opencode-ai/sdk/v2";
import type { Message } from "../model";

/**
 * The seam of the port (spec §23). opencode's renderer draws `Message`/`Part` rows and asks its
 * data context for them; our bench speaks pi events, tool rows, proposals, exchanges and a plan.
 * This file is the only place the two vocabularies meet — everything above it is opencode's code,
 * unedited, and everything below it is ours.
 *
 * It is pure: rows in, rows out, no Solid and no `window`, so the mapping is a table test rather
 * than something you have to look at.
 */

type Row = Message;
type Action = Extract<Message, { role: "action" }>;
type Question = Extract<Message, { role: "question" }>;

/** Ours → theirs, as `opencode-map.ts` already had it; a name they register keeps its own body. */
const TOOLS: Record<string, string> = {
  read: "read",
  ls: "list",
  find: "glob",
  grep: "grep",
  bash: "bash",
  process: "bash",
  write: "write",
  edit: "edit",
  patch: "patch",
  plan: "todowrite",
  skill: "skill",
  question: "question",
};

/**
 * An `ask` is a subagent to them whichever way we use it: `to: "agent"` starts one, and an ask to a
 * workspace is the same shape with the workspace as the agent (§23).
 */
export const toolName = (tool: string | undefined): string => (tool ? (TOOLS[tool] ?? (tool === "ask" ? "task" : tool)) : "");

/** Their agent name for an `ask`: the agent's own name, or the workspace it was sent to. */
export const askAgent = (args: Record<string, unknown> = {}): string =>
  String(args.to === "agent" ? (args.name ?? "general") : (args.to ?? "workspace"));

const ms = (r: { ts?: number }, i: number) => r.ts ?? i;

/** A tool row's state, in their four shapes. A row we are still waiting on is `running`. */
export function toolState(a: Action, i: number): ToolState {
  const input = (a.args ?? {}) as Record<string, unknown>;
  const start = ms(a, i);
  if (a.pending) return { status: "running", input, title: a.text, time: { start } };
  const end = start + (a.ms ?? 0);
  if (a.ok === false) return { status: "error", input, error: a.output ?? "failed", time: { start, end } };
  return { status: "completed", input, output: a.output ?? "", title: a.text, metadata: {}, time: { start, end } };
}

/**
 * A `kl_*` platform answer has no part of their own — their renderer knows nothing of our API — so
 * it is a text part carrying the answer as a fenced block until it earns a part of its own.
 * VENDORED.md keeps the TODO.
 */
export const platformText = (a: Action): string =>
  `\`\`\`json\n${(a.output ?? "").trim() || "{}"}\n\`\`\``;

export type Bundle = { messages: OcMessage[]; parts: Part[] };

/**
 * Our transcript, as their message and part rows. Every user row starts a user message; every
 * assistant row, tool row and divider hangs off the assistant message that follows it, which is
 * how their `groupParts` expects to read a turn.
 */
export function toParts(rows: readonly Row[], opts: { session: string; model?: string; agent?: string; cwd?: string } = { session: "s" }): Bundle {
  const session = opts.session;
  const [providerID = "", modelID = ""] = String(opts.model ?? "").split("/");
  const messages: OcMessage[] = [];
  const parts: Part[] = [];
  let assistant: AssistantMessage | undefined;

  const assistantFor = (r: Row, i: number): AssistantMessage => {
    if (assistant) return assistant;
    assistant = {
      id: `a${i}`,
      sessionID: session,
      role: "assistant",
      time: { created: ms(r, i) },
      parentID: messages.filter((m) => m.role === "user").at(-1)?.id ?? `u${i}`,
      modelID,
      providerID,
      mode: "build",
      agent: opts.agent ?? "build",
      path: { cwd: opts.cwd ?? "", root: opts.cwd ?? "" },
      cost: 0,
      tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
    };
    messages.push(assistant);
    return assistant;
  };
  const close = (at: number) => {
    if (assistant && !assistant.time.completed) assistant.time.completed = at;
    assistant = undefined;
  };

  rows.forEach((r, i) => {
    const time = ms(r, i);
    switch (r.role) {
      case "user": {
        // A person's turn ends whatever the assistant was saying.
        close(time);
        const user: UserMessage = {
          id: `u${i}`,
          sessionID: session,
          role: "user",
          time: { created: time },
          agent: opts.agent ?? "build",
          model: { providerID, modelID },
        };
        messages.push(user);
        parts.push({ id: `p${i}`, sessionID: session, messageID: user.id, type: "text", text: r.text });
        return;
      }
      case "assistant": {
        const m = assistantFor(r, i);
        if (r.interrupted) m.error = { name: "MessageAbortedError", data: {} } as AssistantMessage["error"];
        parts.push(
          r.kind === "reasoning"
            ? { id: `p${i}`, sessionID: session, messageID: m.id, type: "reasoning", text: r.text, time: { start: time } }
            : { id: `p${i}`, sessionID: session, messageID: m.id, type: "text", text: r.text, time: { start: time } },
        );
        return;
      }
      case "divider": {
        const m = assistantFor(r, i);
        parts.push({ id: `p${i}`, sessionID: session, messageID: m.id, type: "compaction" } as Part);
        return;
      }
      case "question": {
        // An answered proposal is the record of what was agreed; the LIVE one is a permission dock,
        // which the pane renders beside the composer rather than in the transcript.
        const m = assistantFor(r, i);
        const state: ToolState = r.answer
          ? { status: "completed", input: r.args ?? {}, output: r.answer, title: r.summary, metadata: {}, time: { start: time, end: time } }
          : { status: "running", input: r.args ?? {}, title: r.summary, time: { start: time } };
        parts.push({ id: `p${i}`, sessionID: session, messageID: m.id, type: "tool", callID: r.id, tool: "question", state } as ToolPart);
        return;
      }
      case "action": {
        const m = assistantFor(r, i);
        if (!r.tool) {
          // A harness note — an agent reporting, a command finishing — is their synthetic text.
          parts.push({ id: `p${i}`, sessionID: session, messageID: m.id, type: "text", text: r.text, synthetic: true });
          return;
        }
        if (r.tool.startsWith("kl_")) {
          parts.push({ id: `p${i}`, sessionID: session, messageID: m.id, type: "text", text: platformText(r), synthetic: true });
          return;
        }
        const part: ToolPart = {
          id: `p${i}`,
          sessionID: session,
          messageID: m.id,
          type: "tool",
          callID: `c${i}`,
          tool: toolName(r.tool),
          state: toolState(r, i),
        };
        if (r.tool === "ask") part.metadata = { agent: askAgent(r.args), background: r.args?.to === "agent" };
        parts.push(part);
        return;
      }
    }
  });
  close(rows.length ? ms(rows[rows.length - 1], rows.length - 1) : 0);
  return { messages, parts };
}

/** The parts of one message, the way their data context is asked for them. */
export const partsOf = (bundle: Bundle, messageID: string): Part[] => bundle.parts.filter((p) => p.messageID === messageID);
