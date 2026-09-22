export type ChoiceAnswer = { type: "choice"; choice: string; confidence: number; probabilities: Record<string, number> };
export type NoulAnswer = { type: "noul"; noul: number };
export type Answer = ChoiceAnswer | NoulAnswer;
export type Question =
  | { type: "choice"; instructions: string; criteria: Record<string, string> }
  | { type: "noul"; instructions: string };
export type Ask = (state: unknown, questions: Record<string, Question>) => Promise<Record<string, Answer>>;

import { addUsage } from "./usage.ts";
import { compress } from "./headroom.ts";

const MIN_CHARS = 800; // headroom's own gate; below it compress() is a no-op anyway, so this check is just to skip the walk

// Trims what's sent to Jev the same way tool output is trimmed for the LLM: every long string field of state,
// one level deep, goes through the same compressor. `questions` is the ask itself, never touched.
function compressState(state: unknown): unknown {
  if (typeof state !== "object" || state === null || Array.isArray(state)) return state;
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(state as Record<string, unknown>)) {
    out[k] = typeof v === "string" && v.length > MIN_CHARS ? compress(v).text : v;
  }
  return out;
}

export const choice = (instructions: string, criteria: Record<string, string>): Question =>
  ({ type: "choice", instructions, criteria });
export const noul = (instructions: string): Question => ({ type: "noul", instructions });

// An outage is waited out, not passed on: seen live, two "503 no healthy upstream" in a row ended a task and lost the work queued behind it.
// A 4xx (bad key, bad request) is ours and is thrown at once.
// ponytail: fixed waits, about 40 seconds in all; a longer outage still fails the step.
export const RETRY_WAITS = [1000, 3000, 8000, 15000];
export const ask: Ask = async (state, questions) => {
  const call = () => fetch("https://api.typesafe.ai/v1/systemone", {
    method: "POST",
    headers: { Authorization: `Bearer ${process.env.TYPESAFE_API_KEY}`, "Content-Type": "application/json" },
    body: JSON.stringify({ state: compressState(state), model: "jev-latest", questions }),
    signal: AbortSignal.timeout(10_000),
  }).catch((e) => e as Error);
  let res = await call();
  for (const wait of RETRY_WAITS) {
    if (!(res instanceof Error) && res.status < 500 && res.status !== 429) break;
    await new Promise((r) => setTimeout(r, wait));
    res = await call();
  }
  if (res instanceof Error) throw new Error(`Jev unreachable: ${res.message}`);
  if (!res.ok) throw new Error(`Jev ${res.status}: ${(await res.text()).slice(0, 200)}`);
  const body = await res.json();
  addUsage("jev", body.usage?.input_tokens, body.usage?.output_tokens);
  return body.answers;
};
