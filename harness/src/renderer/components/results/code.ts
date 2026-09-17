/**
 * Reading a tool result back into rows a code block can draw. Pure, so the parsing is testable
 * without a DOM — and because the bug it fixes was a parsing one: the tool server's `read` already
 * numbers its lines ("12\tconst x = 1"), the viewer added a gutter of its own, and the person saw
 * two columns of numbers with the first line out of step (owner's screenshot, 2026-09-17).
 */
export type CodeLine = { n?: number; text: string };
export type CodeBlock = { lines: CodeLine[]; footer?: string };

/** `[50 lines in all; page with offset]`, `[truncated]`, `[exit 1]` — a trailer, not code. */
const TRAILER = /^\[[^\]]+\]$/;
const NUMBERED = /^\s*(\d+)\t(.*)$/;

/**
 * A numbered listing as the tool server prints it. The numbers are REAL — an offset page starts at
 * its own line — so they are kept rather than re-counted, and the trailer becomes a footer in the
 * block's own chrome rather than a line of fake code.
 */
export function readBlock(out: string, shown?: number): CodeBlock {
  const raw = out.replace(/\s+$/, "").split("\n");
  const trailer = raw.length && TRAILER.test(raw[raw.length - 1].trim()) ? raw.pop()!.trim() : undefined;
  const lines = raw.map((l) => {
    const m = NUMBERED.exec(l);
    return m ? { n: Number(m[1]), text: m[2] } : { text: l };
  });
  const total = trailer && /^\[(\d+) lines in all/.exec(trailer)?.[1];
  const first = lines.find((l) => l.n !== undefined)?.n;
  const last = [...lines].reverse().find((l) => l.n !== undefined)?.n;
  const footer =
    total && first !== undefined && last !== undefined
      ? `${total} lines in all · showing ${first}–${last}`
      : trailer?.replace(/^\[|\]$/g, "");
  return { lines, footer: shown === 0 ? undefined : footer };
}

/** `path:line: text` — grep's own shape, with the path and the number in the gutter. */
export function grepBlock(out: string): { path: string; n: number; text: string }[] {
  return out
    .split("\n")
    .map((l) => /^([^:\s]+):(\d+):(.*)$/.exec(l))
    .filter((m): m is RegExpExecArray => !!m)
    .map((m) => ({ path: m[1], n: Number(m[2]), text: m[3] }));
}

/**
 * Terminal output: no gutter, and no escape codes. A dev server writes colour; a `<pre>` renders
 * the escapes as mojibake, which is worse than plain text and much worse than colour.
 */
// eslint-disable-next-line no-control-regex
const ANSI = /\[[0-?]*[ -/]*[@-~]/g;
export function plainBlock(out: string): CodeBlock {
  const raw = out.replace(ANSI, "").replace(/\s+$/, "").split("\n");
  const trailer = raw.length && TRAILER.test(raw[raw.length - 1].trim()) ? raw.pop()!.trim() : undefined;
  return { lines: raw.map((text) => ({ text })), footer: trailer?.replace(/^\[|\]$/g, "") };
}
