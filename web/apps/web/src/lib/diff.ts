/**
 * A unified diff, parsed into something a table can render.
 *
 * The api serves the diff as text because that is what the git side produces and
 * what stays cheap to cap — a 4 MiB ceiling on a string is one check, a ceiling on
 * a parsed object graph is not. Parsing is therefore the reader's job, here.
 *
 * The format is the one `browse::commit` emits: `--- a/path` and `+++ b/path` per
 * file, then `@@` hunks. No `diff --git` header, and a deleted file still names
 * `a/path` rather than `/dev/null` — it is written for display, not for `git apply`.
 */

export type DiffLine = {
  kind: "add" | "del" | "ctx";
  text: string;
  /** Line number on the left (absent on an added line) and on the right (absent
   *  on a deleted one) — the pair is what makes a diff navigable back to the file. */
  old?: number;
  new?: number;
};
export type Hunk = { header: string; lines: DiffLine[] };
export type FileDiff = {
  path: string;
  hunks: Hunk[];
  additions: number;
  deletions: number;
};

/** Past this many changed lines a file is folded shut by default. A commit that
 *  adds a lockfile should not bury the twelve lines that matter beneath it. */
export const LARGE_FILE = 300;

export type ParsedDiff = {
  files: FileDiff[];
  additions: number;
  deletions: number;
  /** The api stops at its ceiling and says so; the page has to say so too rather
   *  than presenting a partial diff as the whole commit. */
  truncated: boolean;
};

export function parseDiff(diff: string): ParsedDiff {
  const files: FileDiff[] = [];
  let truncated = false;
  let file: FileDiff | undefined;
  let hunk: Hunk | undefined;
  let oldNo = 1;
  let newNo = 1;

  for (const line of diff.split("\n")) {
    if (line === "[diff truncated]") {
      truncated = true;
      continue;
    }
    if (line.startsWith("--- ")) {
      // Opens a file; the `+++` on the next line names the same path.
      hunk = undefined;
      continue;
    }
    if (line.startsWith("+++ ")) {
      file = { path: line.slice(4).replace(/^b\//, ""), hunks: [], additions: 0, deletions: 0 };
      files.push(file);
      hunk = undefined;
      continue;
    }
    if (!file) continue;
    if (line.startsWith("@@")) {
      hunk = { header: line, lines: [] };
      file.hunks.push(hunk);
      // `@@ -oldStart,oldCount +newStart,newCount @@` — the counters run from
      // here, so the numbers are the file's own, not the diff's.
      const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
      oldNo = m ? Number(m[1]) : 1;
      newNo = m ? Number(m[2]) : 1;
      continue;
    }
    if (!hunk) continue;
    if (line.startsWith("+")) {
      hunk.lines.push({ kind: "add", text: line.slice(1), new: newNo++ });
      file.additions++;
    } else if (line.startsWith("-")) {
      hunk.lines.push({ kind: "del", text: line.slice(1), old: oldNo++ });
      file.deletions++;
    } else {
      // A context line starts with a space; a trailing empty string from the
      // final split is not a line at all.
      if (line === "") continue;
      hunk.lines.push({
        kind: "ctx",
        text: line.startsWith(" ") ? line.slice(1) : line,
        old: oldNo++,
        new: newNo++,
      });
    }
  }

  return {
    files,
    additions: files.reduce((n, f) => n + f.additions, 0),
    deletions: files.reduce((n, f) => n + f.deletions, 0),
    truncated,
  };
}
