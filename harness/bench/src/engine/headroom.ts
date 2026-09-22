// !Port of headroom's ContentRouter + CCR (Compress/Cache/Retrieve) into the bench engine.
//
// Upstream headroom (Python, now Rust-backed via crates/headroom-core) routes a tool output
// through a content detector, picks one of several lossy compressors, and stores the original
// bytes behind a short hash so a later CCR tool call can pull it back verbatim. This file ports
// just the RULES that matter for the bench's tool-output path — five detectors (json-array,
// search, diff, log, text) plus the size gate and the in-memory retrieve store — as fresh,
// synchronous TypeScript with no dependency on headroom's Rust extension, its TOIN learning
// loop, cross-turn dedup, adaptive sizing, or the batch/MCP machinery in headroom/ccr/. Those
// are session-lifetime or multi-turn features the bench's one-shot tool calls don't have a use
// for yet.

import { createHash } from "node:crypto";

export type Compressed = { text: string; hash?: string; strategy: string; before: number; after: number };

const MIN_CHARS = 800;
const MAX_ITEMS = 15;
// Same prefix headroom-adjacent code in this repo already treats as "nothing to compress, it's a refusal":
// executor.ts::relevant matches the same shape.
const REFUSAL = /^(blocked|failed|denied|error)/i;

// ponytail: 200-entry in-memory LRU keyed by hash; a SQLite-backed store if a session outgrows one process's memory.
const STORE_CAP = 200;
const store = new Map<string, string>();

function remember(original: string): string {
  const hash = createHash("sha256").update(original).digest("hex").slice(0, 12);
  store.delete(hash);
  store.set(hash, original);
  if (store.size > STORE_CAP) store.delete(store.keys().next().value as string);
  return hash;
}

export function retrieve(hash: string): string | undefined {
  return store.get(hash);
}

function marker(before: number, after: number, hash: string): string {
  return `\n\n[${before} chars compressed to ${after}. Retrieve original: hash=${hash}]`;
}

function pass(text: string): Compressed {
  return { text, strategy: "pass", before: text.length, after: text.length };
}

// ---- json-array ------------------------------------------------------------------------------

function asArray(parsed: unknown): unknown[] | undefined {
  if (Array.isArray(parsed)) return parsed;
  if (parsed && typeof parsed === "object") {
    const arrayKeys = Object.entries(parsed as Record<string, unknown>).filter(([, v]) => Array.isArray(v));
    if (arrayKeys.length === 1) return arrayKeys[0][1] as unknown[];
  }
  return undefined;
}

function sameKeySet(items: unknown[]): string[] | undefined {
  if (items.length === 0 || !items.every((it) => it && typeof it === "object" && !Array.isArray(it))) return undefined;
  const first = Object.keys(items[0] as object).sort();
  const key = first.join("\0");
  for (const it of items) if (Object.keys(it as object).sort().join("\0") !== key) return undefined;
  return first;
}

function cell(v: unknown): string {
  return typeof v === "string" ? v : JSON.stringify(v);
}

function asTable(items: unknown[], original: string): Compressed | undefined {
  const keys = sameKeySet(items);
  if (!keys) return undefined;
  const lines = [`keys: ${keys.join(", ")}`, ...items.map((it) => keys.map((k) => cell((it as Record<string, unknown>)[k])).join(" | "))];
  const text = lines.join("\n");
  return { text, strategy: "table", before: original.length, after: text.length };
}

function words(s: string): Set<string> {
  return new Set((s.toLowerCase().match(/[a-z0-9]{4,}/g) ?? []));
}

function smartSample(items: unknown[], query: string): { kept: unknown[]; strategy: string } {
  const n = items.length;
  const keep = new Set<number>();
  const headN = Math.min(5, Math.ceil(0.3 * n));
  const tailN = Math.min(3, Math.ceil(0.15 * n));
  for (let i = 0; i < headN; i++) keep.add(i);
  for (let i = n - tailN; i < n; i++) if (i >= 0) keep.add(i);
  for (let i = 0; i < n; i++) if (/error|fail|exception/i.test(JSON.stringify(items[i]))) keep.add(i);

  // numeric-anomaly pass: for each field that is numeric across items, flag >2 stddev from the mean
  const fields = new Map<string, number[]>();
  for (const it of items) {
    if (!it || typeof it !== "object") continue;
    for (const [k, v] of Object.entries(it as Record<string, unknown>)) if (typeof v === "number") (fields.get(k) ?? fields.set(k, []).get(k)!).push(v);
  }
  for (const [k, vals] of fields) {
    if (vals.length !== items.length) continue;
    const mean = vals.reduce((a, b) => a + b, 0) / vals.length;
    const sd = Math.sqrt(vals.reduce((a, b) => a + (b - mean) ** 2, 0) / vals.length);
    if (sd === 0) continue;
    items.forEach((it, i) => {
      const v = (it as Record<string, unknown>)[k];
      if (typeof v === "number" && Math.abs(v - mean) > 2 * sd) keep.add(i);
    });
  }

  const qWords = words(query);
  if (qWords.size > 0) items.forEach((it, i) => { for (const w of words(JSON.stringify(it))) if (qWords.has(w)) { keep.add(i); break; } });

  const orderedKept = [...keep].sort((a, b) => a - b).slice(0, MAX_ITEMS);
  return { kept: orderedKept.map((i) => items[i]), strategy: `smart_sample(${n}->${orderedKept.length})` };
}

function tryJsonArray(text: string, query: string): Compressed | undefined {
  let parsed: unknown;
  try { parsed = JSON.parse(text.trim()); } catch { return undefined; }
  const items = asArray(parsed);
  if (!items) return undefined;
  const table = asTable(items, text);
  if (table) return table;
  const { kept, strategy } = smartSample(items, query);
  const body = JSON.stringify(kept);
  return { text: body, strategy, before: text.length, after: body.length };
}

// ---- search (grep -n shape) -------------------------------------------------------------------

const SEARCH_LINE = /^[^\s:]+:\d+[:-]/;

function trySearch(text: string): Compressed | undefined {
  const lines = text.split("\n");
  const nonEmpty = lines.filter((l) => l.length > 0);
  if (nonEmpty.length === 0) return undefined;
  const hitCount = nonEmpty.filter((l) => SEARCH_LINE.test(l)).length;
  if (hitCount / nonEmpty.length < 0.6) return undefined;

  const byFile = new Map<string, string[]>();
  for (const l of nonEmpty) {
    const m = SEARCH_LINE.exec(l);
    const file = m ? l.slice(0, l.indexOf(":")) : "?";
    (byFile.get(file) ?? byFile.set(file, []).get(file)!).push(l);
  }
  const files = [...byFile.entries()].sort((a, b) => b[1].length - a[1].length);
  const out: string[] = [];
  for (const [file, hits] of files) {
    out.push(...hits.slice(0, 3));
    if (hits.length > 3) out.push(`… +${hits.length - 3} more in ${file}`);
  }
  const body = out.join("\n");
  return { text: body, strategy: "search", before: text.length, after: body.length };
}

// ---- diff ---------------------------------------------------------------------------------

function tryDiff(text: string): Compressed | undefined {
  const lines = text.split("\n");
  const looksDiff = lines[0]?.startsWith("diff --git") || (text.includes("---") && text.includes("+++"));
  if (!looksDiff || !text.includes("@@")) return undefined;
  const kept = lines.filter((l) => l.startsWith("diff --git") || l.startsWith("+++") || l.startsWith("---") || l.startsWith("@@") || l.startsWith("+") || l.startsWith("-"));
  let body = kept.join("\n");
  if (body.length > 4000) body = `${body.slice(0, 2000)}…${body.slice(-1500)}`;
  return { text: body, strategy: "diff", before: text.length, after: body.length };
}

// ---- log ------------------------------------------------------------------------------------

const LOG_START = /^\d{4}-\d\d-\d\d|^\[?\d\d:\d\d:\d\d/;
const LOG_LEVEL = /\b(INFO|WARN|ERROR|DEBUG|TRACE)\b/;
const LOG_IMPORTANT = /error|warn|fail|panic|exception/i;

function tryLog(text: string): Compressed | undefined {
  const lines = text.split("\n");
  const matching = lines.filter((l) => LOG_START.test(l) || LOG_LEVEL.test(l)).length;
  if (lines.length === 0 || matching / lines.length < 0.4) return undefined;

  const collapsed: string[] = [];
  let runKey = "";
  let runLine = "";
  let runCount = 0;
  const flush = () => {
    if (runCount === 0) return;
    collapsed.push(runCount > 1 ? `${runLine} (×${runCount})` : runLine);
  };
  for (const l of lines) {
    const key = l.replace(/\d/g, "");
    if (key === runKey) { runCount++; continue; }
    flush();
    runKey = key; runLine = l; runCount = 1;
  }
  flush();

  const important = new Set(lines.filter((l) => LOG_IMPORTANT.test(l)));
  const head = collapsed.slice(0, 10);
  const tail = collapsed.slice(-20);
  const middleImportant = collapsed.slice(10, -20).filter((l) => [...important].some((i) => l.startsWith(i.replace(/\d/g, "")) || l === i));
  const kept = [...new Set([...head, ...middleImportant, ...tail])];
  const body = kept.join("\n");
  return { text: body, strategy: "log", before: text.length, after: body.length };
}

// ---- text (fallback) -----------------------------------------------------------------------

function tryText(text: string): Compressed {
  const lines = text.split("\n");
  const deduped = [...new Set(lines)];
  let out = deduped;
  if (deduped.length > 80) {
    const head = deduped.slice(0, 40);
    const tail = deduped.slice(-20);
    out = [...head, `… [${deduped.length - 60} lines omitted] …`, ...tail];
  }
  const body = out.join("\n");
  return { text: body, strategy: "text", before: text.length, after: body.length };
}

// ---- entry point ----------------------------------------------------------------------------

export function compress(text: string, query = ""): Compressed {
  if (text.length < MIN_CHARS || REFUSAL.test(text)) return pass(text);

  let result: Compressed;
  try {
    result = tryJsonArray(text, query) ?? trySearch(text) ?? tryDiff(text) ?? tryLog(text) ?? tryText(text);
  } catch {
    return pass(text);
  }

  if (result.after >= result.before * 0.8) return pass(text); // overhead would exceed savings
  if (result.strategy === "table") return result; // lossless — nothing to retrieve

  const hash = remember(text);
  return { ...result, hash, text: result.text + marker(result.before, result.after, hash) };
}

// ---- CCR retrieve tool ------------------------------------------------------------------------

export const RETRIEVE_TOOL = {
  name: "retrieve",
  label: "retrieve",
  description: "Return the original of a tool output that was compressed; hash is in the marker line.",
  parameters: { type: "object", properties: { hash: { type: "string" } }, required: ["hash"] },
  execute: async (_id: string, input: { hash: string }) => {
    const original = retrieve(input.hash);
    return { content: [{ type: "text" as const, text: original ?? "unknown hash" }], details: {} };
  },
};
