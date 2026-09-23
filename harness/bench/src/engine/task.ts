export type Envelope = { instruction: string; responses: string[]; context: unknown[]; full?: string[]; depth: number; messages: Record<string, string>; generate?: string[]; generateTool?: string; command?: string };

// A task's plan step reaches its params writer at depth 4; the writer's own lookups (a read, and that read's params) need two more.
export const MAX_DEPTH = 6;
// A frame is kept twice. `context` is what the LLM's params prompt reads, cut short because its input tokens cost. `full` is what Jev reads:
// Jev is free, and probed live a whole earlier step turned a wrong file vote ("none") into the right one. The root frame carries main's
// recent turns, so it gets more room. ROOT_FRAME_CAP plus MAX_DEPTH frames of JEV_FRAME_CAP stay under 60,000 chars, inside Jev's 32,000-token state limit.
const FRAME_CAP = 2000;
export const JEV_FRAME_CAP = 5000;
const ROOT_FRAME_CAP = 24_000;

const cap = (frame: unknown) => JSON.stringify(frame).slice(0, FRAME_CAP);
const capFull = (frame: unknown) => JSON.stringify(frame).slice(0, JEV_FRAME_CAP);
// What Jev reads of an envelope's chain.
export const chain = (e: Envelope) => e.full ?? e.context;

// The generic set every envelope starts with; folds what REPLIES used to hold alone.
export const GENERIC_MESSAGES: Record<string, string> = {
  done: "Done.",
  failed: "Failed: {detail}",
  blocked: "Blocked: {detail}",
  chat: "Hi. Tell me what to do.",
};

export function root(instruction: string, frame: unknown, responses: string[], messages: Record<string, string> = GENERIC_MESSAGES): Envelope {
  return { instruction, responses, context: [cap(frame)], full: [JSON.stringify(frame).slice(0, ROOT_FRAME_CAP)], depth: 0, messages };
}

export class TooDeep extends Error {}

// A child inherits the parent's messages and adds/overrides with its own (e.g. a tool's outcome templates).
export function child(parent: Envelope, instruction: string, frame: unknown, responses: string[], messages: Record<string, string> = {}, fullFrame: unknown = frame): Envelope {
  if (parent.depth >= MAX_DEPTH) throw new TooDeep("blocked: too deep; asking again will not help, go on with what you have");
  return { instruction, responses, context: [...parent.context, cap(frame)], full: [...chain(parent).map(String), capFull(fullFrame)], depth: parent.depth + 1, messages: { ...parent.messages, ...messages } };
}

// Last 5 lines of tool output, for the {tail} slot.
export const tail = (out: string) => out.split("\n").slice(-5).join("\n");

// Fills {tail} and {detail} in a message template.
export function fillTemplate(template: string, slots: { tail?: string; detail?: string }): string {
  return template.replace(/\{tail\}/g, slots.tail ?? "").replace(/\{detail\}/g, slots.detail ?? "");
}

export type Parsed = { response: string; detail: string };

// First word of a final answer is the response; "done" has no detail, others carry "x" after ": ".
export function parseResponse(text: string, responses: string[]): Parsed {
  const m = text.match(/^(\w+):\s*(.*)$/s);
  if (m && responses.includes(m[1])) return { response: m[1], detail: m[2] };
  const bare = text.trim().split(/\s+/)[0];
  if (bare && responses.includes(bare)) return { response: bare, detail: "" };
  return { response: "other", detail: text };
}
