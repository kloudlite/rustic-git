import type { Message } from "../../model";

type Action = Extract<Message, { role: "action" }>;

/**
 * Consecutive tool calls of ONE turn, folded into one group row (§20). pi runs the sibling tool
 * calls of an assistant message concurrently by default, so six commands are six rows that all
 * start at once and finish out of order — six separate rows read as six separate decisions, which
 * is not what happened.
 *
 * Pure, so the grouping is a table test rather than a screenshot.
 */
export type Segment = { kind: "one"; row: Message } | { kind: "group"; rows: Action[] };

const isTool = (m: Message): m is Action => m.role === "action" && !!(m as Action).tool;

/** Runs of two or more tool rows become a group; anything else passes through unchanged. */
export function segments(blocks: Message[]): Segment[] {
  const out: Segment[] = [];
  let run: Action[] = [];
  const flush = () => {
    if (run.length > 1) out.push({ kind: "group", rows: run });
    else if (run.length === 1) out.push({ kind: "one", row: run[0] });
    run = [];
  };
  for (const b of blocks) {
    if (isTool(b)) run.push(b);
    else {
      flush();
      out.push({ kind: "one", row: b });
    }
  }
  flush();
  return out;
}

const SHELL = new Set(["bash", "process", "kl_container_build"]);
const READ = new Set(["read", "ls"]);
const SEARCH = new Set(["grep", "find"]);

/**
 * What the group is doing, in the person's words: six of a kind is named by that kind, a mixture is
 * named "tool calls" rather than by whichever happened to be first.
 */
export function verb(rows: Action[]): string {
  const kinds = new Set(rows.map((r) => (SHELL.has(r.tool ?? "") ? "shell" : READ.has(r.tool ?? "") ? "read" : SEARCH.has(r.tool ?? "") ? "search" : "other")));
  const n = rows.length;
  if (kinds.size > 1) return `${n} tool calls`;
  const [only] = [...kinds];
  return only === "shell" ? `${n} shell command${n === 1 ? "" : "s"}` : only === "read" ? `${n} read${n === 1 ? "" : "s"}` : only === "search" ? `${n} search${n === 1 ? "" : "es"}` : `${n} tool calls`;
}

/** A group is running while any row is; its clock is the FIRST start to the last end. */
export function timing(rows: Action[], now: number): { running: boolean; ms: number } {
  const running = rows.some((r) => r.pending);
  const began = Math.min(...rows.map((r) => r.ts ?? now));
  const ended = running ? now : Math.max(...rows.map((r) => (r.ts ?? now) + (r.ms ?? 0)));
  return { running, ms: Math.max(0, ended - began) };
}

/** `5m 10s`, `1.4s` — a group's clock is read at a glance, not to the millisecond. */
export function elapsed(ms: number): string {
  const s = ms / 1000;
  if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)}s`;
  return `${Math.floor(s / 60)}m ${Math.round(s % 60)}s`;
}
