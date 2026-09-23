import { noul, type Ask, type NoulAnswer } from "./jev.ts";

export type Block = { start: number; end: number; text: string; score?: number };

export function splitBlocks(text: string, max = 1500): Block[] {
  const lines = text.split("\n");
  // Group lines into paragraphs separated by blank lines, tracking 1-based line numbers.
  const paras: Block[] = [];
  let start = -1;
  let buf: string[] = [];
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].trim() === "") {
      // A blank line inside an indented body (a function, a list) is not a boundary: only an unindented line starts a new block.
      const next = lines.slice(i + 1).find((l) => l.trim() !== "");
      if (buf.length > 0 && next !== undefined && /^\s/.test(next)) { buf.push(lines[i]); continue; }
      if (buf.length > 0) { paras.push({ start, end: i, text: buf.join("\n") }); buf = []; start = -1; }
    } else {
      if (start === -1) start = i + 1;
      buf.push(lines[i]);
    }
  }
  if (buf.length > 0) paras.push({ start, end: lines.length, text: buf.join("\n") });

  // Cut any paragraph longer than max at line boundaries.
  const cut: Block[] = [];
  for (const p of paras) {
    if (p.text.length <= max) { cut.push(p); continue; }
    const pLines = p.text.split("\n");
    let s = p.start, buf2: string[] = [], len = 0;
    for (let i = 0; i < pLines.length; i++) {
      const l = pLines[i];
      if (buf2.length > 0 && len + 1 + l.length > max) {
        cut.push({ start: s, end: s + buf2.length - 1, text: buf2.join("\n") });
        s = s + buf2.length; buf2 = []; len = 0;
      }
      buf2.push(l); len += (buf2.length > 1 ? 1 : 0) + l.length;
    }
    if (buf2.length > 0) cut.push({ start: s, end: s + buf2.length - 1, text: buf2.join("\n") });
  }

  // Merge adjacent small blocks. ponytail: the merge size scales with the text (about 16 blocks minimum) so a small file is not one block; a syntax-aware splitter is the upgrade.
  const mergeMax = Math.min(max, Math.max(200, Math.floor(text.length / 16)));
  const merged: Block[] = [];
  for (const b of cut) {
    const last = merged[merged.length - 1];
    if (last) {
      const gap = "\n".repeat(b.start - last.end);
      const combinedText = last.text + gap + b.text;
      if (combinedText.length <= mergeMax) { merged[merged.length - 1] = { start: last.start, end: b.end, text: combinedText }; continue; }
    }
    merged.push(b);
  }
  return merged;
}

const cutQuery = (q: string) => q.length > 60 ? q.slice(0, 60) + "…" : q;

const MAX_CALLS = 24; // Jev calls per pick (0.3-0.9s each); past it the result is marked truncated
const BLOCK = 1500;     // chars in one block, the unit scored and returned. Probed live: at 800 a function is cut mid-body and 2 of 5 targets were missed, at 1500 none
const PER_CALL = 24;    // blocks scored per Jev call
const CALL_CHARS = 30_000; // chars of block text per call, well inside Jev's 32,000-token state limit
const KEEP = 0.5;       // a block at or above this is returned
const MARGIN = 0.15;    // kept blocks scoring this far under the best one are dropped
const LINES = 16;       // lines scored per Jev call in lines mode

// Every block is scored on its own whole text, in one flat pass. The picker searches inside one piece of content, so Jev is handed the query
// and the blocks and nothing else. Probed live on a 41,000-char file: scoring groups on a digest of first lines missed 2 of 5 targets in
// 4 to 20 calls (a target below its block's first line was never seen); whole blocks found 5 of 5 in 2 calls.
export async function pickBlocks(ask: Ask, text: string, query: string, opts?: { trace?: (l: string) => void; maxCalls?: number; lines?: boolean }): Promise<{ blocks: Block[]; calls: number; truncated: boolean }> {
  // Search hits are unrelated lines: grouped, one real hit among noise drags the whole block under KEEP, so each line is its own block.
  const all = opts?.lines ? text.split("\n").map((l, i) => ({ start: i + 1, end: i + 1, text: l })).filter((b) => b.text.trim() !== "") : splitBlocks(text, BLOCK);
  const maxCalls = opts?.maxCalls ?? MAX_CALLS;
  const trace = opts?.trace ?? (() => {});
  let calls = 0;
  let truncated = false;
  const kept: Block[] = [];

  async function score(batch: Block[]) {
    if (calls >= maxCalls) { truncated = true; return; }
    calls++;
    const span = (b: Block) => `L${b.start}-${b.end}`;
    const state: Record<string, string> = { query };
    batch.forEach((b, i) => { state[`c${i + 1}`] = `${span(b)}\n${b.text}`; });
    let scores: number[];
    try {
      const res = await ask(state, Object.fromEntries(batch.map((_, i) => [`c${i + 1}`, noul(`Judge only the text under "c${i + 1}": is it relevant to the query?`)])));
      scores = batch.map((_, i) => (res[`c${i + 1}`] as NoulAnswer)?.noul ?? 0);
    } catch (e) { trace(`[picked] jev failed: ${(e as Error).message.slice(0, 120)}`); scores = batch.map(() => 0); }
    trace(`[picked] ${batch.map((b, i) => `${span(b)} ${scores[i].toFixed(2)}`).join(" | ")}`);
    batch.forEach((b, i) => { if (scores[i] >= KEEP) kept.push({ ...b, score: scores[i] }); });
  }

  trace(`pick "${cutQuery(query)}": ${all.length} blocks`);
  const perCall = opts?.lines ? LINES : PER_CALL;
  const batches: Block[][] = [[]];
  let chars = 0;
  for (const b of all) {
    const cur = batches[batches.length - 1];
    if (cur.length >= perCall || (cur.length > 0 && chars + b.text.length > CALL_CHARS)) { batches.push([b]); chars = b.text.length; }
    else { cur.push(b); chars += b.text.length; }
  }
  await Promise.all(batches.filter((b) => b.length > 0).map(score));
  const best = Math.max(0, ...kept.map((b) => b.score ?? 0));
  const lines = text.split("\n");
  const out: Block[] = [];
  for (const b of kept.filter((k) => (k.score ?? 0) >= best - MARGIN).sort((a, b) => a.start - b.start)) {
    const last = out[out.length - 1];
    // Adjacent pieces join back into one range.
    if (last && b.start <= last.end + 1) { last.end = b.end; last.text = lines.slice(last.start - 1, last.end).join("\n"); last.score = Math.max(last.score ?? 0, b.score ?? 0); }
    else out.push(b);
  }
  kept.splice(0, kept.length, ...out);
  trace(`kept ${kept.length} blocks, ${calls} jev calls`);
  return { blocks: kept, calls, truncated };
}

export function formatBlocks(source: string, blocks: Block[]): string {
  return blocks.map((b) => {
    const lines = b.text.split("\n");
    const width = String(b.end).length;
    const body = lines.map((l, i) => `${String(b.start + i).padStart(width)}| ${l}`).join("\n");
    return `${source}:${b.start}-${b.end}${b.score === undefined ? "" : ` (${b.score.toFixed(2)})`}\n${body}`;
  }).join("\n\n");
}
