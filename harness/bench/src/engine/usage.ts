// Running token totals for the process: what went to and came back from the LLM and from Jev.
type Total = { calls: number; input: number; output: number };
export const usage: Record<"llm" | "jev", Total> = { llm: { calls: 0, input: 0, output: 0 }, jev: { calls: 0, input: 0, output: 0 } };

export let cached = 0; // LLM input tokens served from the provider's prompt cache, counted inside "in" as well

export function addUsage(kind: "llm" | "jev", input = 0, output = 0) {
  const t = usage[kind];
  t.calls++; t.input += input; t.output += output;
}

// LLM input by who asked (params writer, planner, drafter, task): where the tokens go, so a trim can be sized before it is built.
export const byKind: Record<string, Total> = {};

export const usageLine = () =>
  [...(["llm", "jev"] as const).map((k) => `${k} ${usage[k].calls} calls, ${usage[k].input} in${k === "llm" && cached ? ` (${cached} cached)` : ""}, ${usage[k].output} out`),
    ...Object.entries(byKind).map(([k, t]) => `${k} ${t.calls} calls, ${t.input} in (${Math.round(t.input / t.calls)}/call), ${t.output} out`)].join(" | ");

// One LLM request; input includes the cached part.
export function trackUsage(kind: string, input = 0, output = 0, cacheRead = 0) {
  cached += cacheRead;
  addUsage("llm", input, output);
  const t = byKind[kind] ??= { calls: 0, input: 0, output: 0 };
  t.calls++; t.input += input; t.output += output;
}
