import type { Message } from "../../model";

type Action = Extract<Message, { role: "action" }>;

/**
 * The one live line that says what is happening right now — "Reading 1 file, listing 1 directory,
 * running 1 shell command…" — and the same line in the past tense when it is over (§21, observed
 * in Claude Code). It replaces a row per call with a sentence a person reads at a glance.
 *
 * Pure, because the grammar is the whole trick: a count, a verb that agrees with it, and the same
 * verb in the past tense when the work is done.
 */
type Shape = { one: string; many: string; did: string };
const SHAPES: Record<string, Shape> = {
  read: { one: "file", many: "files", did: "Read" },
  ls: { one: "directory", many: "directories", did: "Listed" },
  grep: { one: "search", many: "searches", did: "Searched" },
  find: { one: "search", many: "searches", did: "Searched" },
  bash: { one: "shell command", many: "shell commands", did: "Ran" },
  process: { one: "process", many: "processes", did: "Managed" },
  write: { one: "file", many: "files", did: "Wrote" },
  edit: { one: "file", many: "files", did: "Edited" },
  ask: { one: "agent", many: "agents", did: "Asked" },
};
const DOING: Record<string, string> = { read: "Reading", ls: "listing", grep: "searching", find: "searching", bash: "running", process: "managing", write: "writing", edit: "editing", ask: "asking" };
const OTHER: Shape = { one: "tool call", many: "tool calls", did: "Made" };

/** `Reading 1 file, listing 1 directory, running 1 shell command` / `Read 1 file, listed 1 directory…` */
export function summary(rows: Action[], past = false): string {
  if (!rows.length) return "";
  // Aggregated by KIND, not by call: four agents dispatched in one turn read as "Asked 4 agents",
  // never as "made 1 tool call" four times over (owner, 2026-09-17).
  // By KIND: every tool this app has no noun for is ONE bucket, not one bucket each. Three
  // platform calls read as "Made 3 tool calls" — they came out as "Made 2 tool calls, made 1 tool
  // call, made 1 tool call" because each name counted separately (owner, 2026-09-17).
  const byTool = new Map<string, number>();
  for (const r of rows) {
    const key = SHAPES[r.tool ?? ""] ? r.tool! : "";
    byTool.set(key, (byTool.get(key) ?? 0) + 1);
  }
  const parts = [...byTool].map(([tool, n], i) => {
    const s = SHAPES[tool] ?? OTHER;
    const noun = `${n} ${n === 1 ? s.one : s.many}`;
    if (past) return `${i === 0 ? s.did : s.did.toLowerCase()} ${noun}`;
    const verb = DOING[tool] ?? "making";
    return `${i === 0 ? verb[0].toUpperCase() + verb.slice(1) : verb} ${noun}`;
  });
  return parts.join(", ");
}

/** The `⎿` sub-lines: what each call is actually on, at most a handful. */
export function subjects(rows: Action[], max = 4): string[] {
  return rows.slice(0, max).map((r) => r.text || r.target || r.tool || "").filter(Boolean);
}

/**
 * The spinner's verb. Claude Code rotates a whimsical list; the point is that a changing word says
 * "still alive" where a fixed one says nothing. It rotates on the elapsed second, so it is stable
 * across re-renders and needs no state.
 */
export const VERBS = [
  "Thinking", "Pondering", "Noodling", "Embellishing", "Percolating", "Ruminating", "Considering",
  "Deliberating", "Cogitating", "Mulling", "Puzzling", "Scheming", "Contemplating", "Brewing",
  "Conjuring", "Tinkering", "Untangling", "Whirring", "Synthesising", "Divining", "Marinating",
  "Sculpting", "Simmering", "Chewing",
];
export const verbAt = (secs: number) => VERBS[Math.floor(Math.max(0, secs) / 4) % VERBS.length];

/** `3s · ↓ 123 tokens · thought for 1s` — only the parts that are actually known. */
export function spinnerMeta(secs: number, tokens?: number, thoughtMs?: number): string {
  const bits = [`${secs}s`];
  if (tokens) bits.push(`↓ ${tokens > 1000 ? `${Math.round(tokens / 100) / 10}k` : tokens} tokens`);
  if (thoughtMs && thoughtMs > 500) bits.push(`thought for ${Math.round(thoughtMs / 1000)}s`);
  return bits.join(" · ");
}

/** `✻ Crunched for 9s · done 3:33 PM · 1 shell still running` */
export function turnFooter(ms: number, at: Date, stillRunning: number): string {
  const secs = Math.max(1, Math.round(ms / 1000));
  const time = at.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  return `Crunched for ${secs}s · done ${time}${stillRunning ? ` · ${stillRunning} still running` : ""}`;
}

/**
 * A message the harness delivers — an agent's report, a finished command, an answered question —
 * is a ROW of its own, not a prompt the person appears to have typed (§21).
 */
export function notification(text: string): { verb: string; detail?: string } | undefined {
  const t = String(text);
  let m = /^\[from agent ([^\]]+)\]\s*([\s\S]*)$/.exec(t);
  if (m) return { verb: `Agent "${m[1]}" finished`, detail: m[2].split("\n")[0] };
  m = /^\[info from ([^\]]+)\]\s*([\s\S]*)$/.exec(t);
  if (m) return { verb: `${m[1]} answered`, detail: m[2].split("\n")[0] };
  m = /^\[from workspace ([^\]]+)\]\s*([\s\S]*)$/.exec(t);
  if (m) return { verb: `${m[1]} replied`, detail: m[2].split("\n")[0] };
  m = /^\[task ([^\]]+) finished: exit ([^\]]+)\]/.exec(t);
  if (m) return { verb: `Background command "${m[1]}" completed (exit code ${m[2]})` };
  m = /^\[watch ([^\]]+) \/([^/]*)\/\]/.exec(t);
  if (m) return { verb: `${m[1]} matched /${m[2]}/`, detail: t.split("\n")[1] };
  m = /^\[harness\]\s*([\s\S]*)$/.exec(t);
  if (m) return { verb: "Harness", detail: m[1] };
  return undefined;
}
