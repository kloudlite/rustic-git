import { readFileSync } from "node:fs";
import { resolve } from "node:path";

export type DiffLine = {
  /** Original-file line number ("" for pure additions past the hunk start). */
  no: number | "";
  sign: " " | "+" | "-";
  text: string;
  /**
   * Full-file view only: "added" = new since HEAD (green line number);
   * "deleted-gap" = a marker row standing in for lines removed here.
   */
  mark?: "added" | "deleted-gap";
  /** deleted-gap: how many lines were removed. */
  count?: number;
};

export type FileDiff = {
  path: string;
  lines: DiffLine[];
  added: number;
  removed: number;
};

const CONTEXT = 3;

/**
 * Hunks for pi's edit tool ({path, edits:[{oldText,newText}]}): context lines
 * from the file around each replacement, removed/added lines numbered like a
 * unified diff. Returns null when the file or a match can't be found.
 */
function editDiff(args: {
  path?: string;
  edits?: { oldText: string; newText: string }[];
}): FileDiff | null {
  if (!args?.path || !Array.isArray(args.edits) || args.edits.length === 0) return null;
  let content: string;
  try {
    content = readFileSync(resolve(args.path), "utf8");
  } catch {
    return null;
  }
  const fileLines = content.split("\n");
  const lines: DiffLine[] = [];
  let added = 0;
  let removed = 0;

  for (const [i, edit] of args.edits.entries()) {
    const at = content.indexOf(edit.oldText);
    if (at === -1) return null;
    const startLine = content.slice(0, at).split("\n").length; // 1-based
    // expand to full lines so indentation and partial-line edits render right
    const lineStart = content.lastIndexOf("\n", at - 1) + 1;
    let matchEnd = at + edit.oldText.length;
    let lineEnd = content.indexOf("\n", matchEnd);
    if (lineEnd === -1) lineEnd = content.length;
    const oldLines = content.slice(lineStart, lineEnd).split("\n");
    const newLines = (
      content.slice(lineStart, at) + edit.newText + content.slice(matchEnd, lineEnd)
    ).split("\n");
    // trim the shared single-line prefix case: numbers stay aligned anyway
    const ctxFrom = Math.max(1, startLine - CONTEXT);
    const afterStart = startLine + oldLines.length;
    const ctxTo = Math.min(fileLines.length, afterStart + CONTEXT - 1);

    if (i > 0) lines.push({ no: "", sign: " ", text: "⋯" });
    for (let n = ctxFrom; n < startLine; n++)
      lines.push({ no: n, sign: " ", text: fileLines[n - 1] ?? "" });
    oldLines.forEach((text, k) =>
      lines.push({ no: startLine + k, sign: "-", text }),
    );
    newLines.forEach((text, k) =>
      lines.push({ no: startLine + k, sign: "+", text }),
    );
    for (let n = afterStart; n <= ctxTo; n++)
      lines.push({ no: n, sign: " ", text: fileLines[n - 1] ?? "" });
    removed += oldLines.length;
    added += newLines.length;
  }
  return { path: args.path, lines, added, removed };
}

/** All-additions diff for pi's write tool ({path, content}). */
function writeDiff(args: { path?: string; content?: string }): FileDiff | null {
  if (!args?.path || typeof args.content !== "string") return null;
  const newLines = args.content.split("\n");
  return {
    path: args.path,
    lines: newLines.map((text, i) => ({ no: i + 1, sign: "+", text })),
    added: newLines.length,
    removed: 0,
  };
}

/** Diff for whichever mutating tool this is, or null. */
export function toolDiff(name: string, args: any): FileDiff | null {
  if (name === "edit") return editDiff(args);
  if (name === "write") return writeDiff(args);
  return null;
}
