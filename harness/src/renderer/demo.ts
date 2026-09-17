/**
 * A canned turn, replayed into a thread's own event handler. Every animation in this app is
 * triggered by an event from pi, so the only way to see one used to be to spend a model turn on it
 * — and an animation nobody can look at is an animation nobody can judge (owner, 2026-09-17).
 *
 * It writes through the same `onEvent` the bench's socket writes through, so what plays is the real
 * path, not a mock of it. Nothing is sent anywhere.
 */
export type Emit = (ev: Record<string, unknown>) => void;

const TEXT =
  "Here is what I found. The status write bumps no generation, so the owner's binding woke on every " +
  "echo — two quota reads and three applies for nothing. The watch filter below is the whole fix, and " +
  "it is one line in `run.rs`.";

/** `[delay from the last step, what to send]`, in the order pi would send them. */
const STEPS: [number, (id: string) => Record<string, unknown>][] = [
  [0, () => ({ type: "agent_start" })],
  [0, () => ({ type: "message_start", message: { role: "user", content: "why does the binding wake so often?", timestamp: Date.now() } })],
  [200, () => ({ type: "message_update", assistantMessageEvent: { type: "thinking_delta", delta: "The watch is unfiltered, so status echoes reach it…" } })],
  [500, () => ({ type: "tool_execution_start", toolCallId: "d1", toolName: "grep", args: { pattern: "watches", path: "bins/agent/src" } })],
  [400, () => ({ type: "tool_execution_start", toolCallId: "d2", toolName: "read", args: { path: "bins/agent/src/controller/run.rs" } })],
  [600, () => ({ type: "tool_execution_end", toolCallId: "d1", result: { output: "bins/agent/src/controller/run.rs:24: .watches(" } })],
  [300, () => ({ type: "tool_execution_end", toolCallId: "d2", result: { output: "   1\tuse crate::{crd, Ctx};" } })],
  [300, () => ({ type: "tool_execution_start", toolCallId: "d3", toolName: "bash", args: { command: "cargo test -p kloudlite-agent binding" } })],
  [900, () => ({ type: "tool_execution_end", toolCallId: "d3", result: { output: "running 3 tests\ntest binding::echo ... ok\n[exit 0]" } })],
  [200, () => ({ type: "message_update", assistantMessageEvent: { type: "text_delta", delta: TEXT }, usage: { totalTokens: 85100, contextWindow: 900000, cost: { total: 0.0114 } } })],
  [400, () => ({ type: "tool_execution_start", toolCallId: "d4", toolName: "edit", args: { path: "bins/agent/src/controller/run.rs", edits: [{ oldText: "        .watches(", newText: "        .watches_stream(" }] } })],
  [700, () => ({ type: "tool_execution_end", toolCallId: "d4", result: { output: "Successfully edited" } })],
  [300, (id) => ({ type: "queue_update", session: id, steering: ["and run the fleet probe after"], followUp: [] })],
  [800, () => ({ type: "agent_end", messages: [] })],
];

/** Plays the turn; returns a stop, because a demo that cannot be stopped is a bug of its own. */
export function playDemo(session: string, emit: Emit): () => void {
  const timers: ReturnType<typeof setTimeout>[] = [];
  let at = 0;
  for (const [delay, step] of STEPS) {
    at += delay;
    timers.push(setTimeout(() => emit({ ...step(session), session }), at));
  }
  return () => timers.forEach(clearTimeout);
}

/** `?motion-demo` in the window's own URL plays it once on load, for a screenshot. */
export const wantsDemo = (search: string): boolean => new URLSearchParams(search).has("motion-demo");
