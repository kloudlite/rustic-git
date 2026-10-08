import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import type { DiffLine, FileDiff } from "./diff.ts";

export type ChangeStatus = "M" | "A" | "D";
export type Change = { path: string; status: ChangeStatus; added: number; removed: number };

export type TreeNode = {
  name: string;
  path: string; // relative to root
  dir: boolean;
  ignored?: boolean;
  children?: TreeNode[]; // loaded lazily for dirs
};

const IGNORED = new Set(["node_modules", ".git", "dist", ".turbo", ".next", "target"]);

function run(root: string, args: string[]): string {
  const p = Bun.spawnSync(["git", ...args], { cwd: root, stdout: "pipe", stderr: "pipe" });
  return p.exitCode === 0 ? p.stdout.toString() : "";
}

export function isGitRepo(root: string): boolean {
  return run(root, ["rev-parse", "--is-inside-work-tree"]).trim() === "true";
}

/** Changed files vs HEAD: tracked modifications/deletions + untracked as added. */
export function changes(root: string): Change[] {
  if (!isGitRepo(root)) return [];
  const out: Change[] = [];
  const stat = run(root, ["diff", "--numstat", "--relative", "HEAD", "--"]);
  for (const line of stat.split("\n")) {
    const [a, r, path] = line.split("\t");
    if (!path) continue;
    let status: ChangeStatus = "M";
    try {
      statSync(join(root, path));
    } catch {
      status = "D";
    }
    out.push({ path, status, added: Number(a) || 0, removed: Number(r) || 0 });
  }
  const untracked = run(root, ["ls-files", "--others", "--exclude-standard"]);
  for (const path of untracked.split("\n").filter(Boolean)) {
    let added = 0;
    try {
      added = readFileSync(join(root, path), "utf8").split("\n").length;
    } catch {}
    out.push({ path, status: "A", added, removed: 0 });
  }
  const order = { M: 0, A: 1, D: 2 };
  return out.sort((x, y) => order[x.status] - order[y.status] || x.path.localeCompare(y.path));
}

/** Parse `git diff` unified output into DiffLines (original-file numbering). */
function parseUnified(text: string): DiffLine[] {
  const lines: DiffLine[] = [];
  let oldNo = 0;
  let newNo = 0;
  let first = true;
  for (const raw of text.split("\n")) {
    if (raw.startsWith("@@")) {
      const m = /@@ -(\d+)(?:,\d+)? \+(\d+)/.exec(raw);
      oldNo = m ? Number(m[1]) : 0;
      newNo = m ? Number(m[2]) : 0;
      if (!first) lines.push({ no: "", sign: " ", text: "⋯" });
      first = false;
      continue;
    }
    if (raw.startsWith("diff ") || raw.startsWith("index ") || raw.startsWith("--- ") || raw.startsWith("+++ ")) continue;
    if (raw.startsWith("\\")) continue; // "\ No newline at end of file"
    if (raw === "" && lines.length === 0) continue;
    const sign = raw[0];
    const body = raw.slice(1);
    // numbers follow the file each line belongs to: additions and context use
    // the new file, removals the old one — so a diff reads like the result
    if (sign === "+") lines.push({ no: newNo++, sign: "+", text: body });
    else if (sign === "-") lines.push({ no: oldNo++, sign: "-", text: body });
    else if (sign === " ") {
      lines.push({ no: newNo, sign: " ", text: body });
      oldNo++;
      newNo++;
    }
  }
  return lines;
}

/** Diff of one path vs HEAD (untracked → all additions). */
export function fileDiff(root: string, path: string, status: ChangeStatus): FileDiff | null {
  if (status === "A") {
    let content = "";
    try {
      content = readFileSync(join(root, path), "utf8");
    } catch {
      return null;
    }
    const ls = content.split("\n");
    return { path, lines: ls.map((text, i) => ({ no: i + 1, sign: "+", text })), added: ls.length, removed: 0 };
  }
  const out = run(root, ["diff", "--relative", "HEAD", "--", path]);
  if (!out) return null;
  const lines = parseUnified(out);
  return {
    path,
    lines,
    added: lines.filter((l) => l.sign === "+").length,
    removed: lines.filter((l) => l.sign === "-").length,
  };
}

/**
 * Which lines of the working copy are new since HEAD, and where lines were
 * deleted. Uses a zero-context diff so the ranges are exact.
 */
function marks(root: string, path: string, status?: ChangeStatus): {
  added: Set<number>;
  all: boolean;
  deletedAbove: Map<number, number>;
} {
  const added = new Set<number>();
  const deletedAbove = new Map<number, number>();
  if (!status || !isGitRepo(root)) return { added, all: false, deletedAbove };
  if (status === "A") return { added, all: true, deletedAbove };
  const out = run(root, ["diff", "-U0", "--relative", "HEAD", "--", path]);
  let newNo = 0;
  for (const raw of out.split("\n")) {
    const hunk = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))?/.exec(raw);
    if (hunk) {
      const start = Number(hunk[1]);
      const count = hunk[2] === undefined ? 1 : Number(hunk[2]);
      // a pure deletion is reported as "+N,0": N is the line it follows, so
      // the removed lines sit before N+1
      newNo = count === 0 ? start + 1 : start;
      continue;
    }
    if (raw.startsWith("+++") || raw.startsWith("---")) continue;
    if (raw.startsWith("+")) added.add(newNo++);
    else if (raw.startsWith("-")) deletedAbove.set(newNo, (deletedAbove.get(newNo) ?? 0) + 1);
  }
  return { added, all: false, deletedAbove };
}

/** Full file, numbered, with new lines and deletion points marked. */
export function fullFile(root: string, path: string, status?: ChangeStatus): DiffLine[] {
  let content = "";
  try {
    content = readFileSync(join(root, path), "utf8");
  } catch {
    return [{ no: "", sign: " ", text: "(binary or unreadable)" }];
  }
  const { added, all, deletedAbove } = marks(root, path, status);
  const out: DiffLine[] = [];
  content.split("\n").forEach((text, i) => {
    const no = i + 1;
    // lines removed here show as one marker row, so the file itself stays intact
    const gone = deletedAbove.get(no);
    if (gone) out.push({ no: "", sign: " ", text: "", mark: "deleted-gap", count: gone });
    out.push({ no, sign: " ", text, mark: all || added.has(no) ? "added" : undefined });
  });
  // deletions at the very end of the file
  const tail = deletedAbove.get(content.split("\n").length + 1);
  if (tail) out.push({ no: "", sign: " ", text: "", mark: "deleted-gap", count: tail });
  return out;
}

/** One directory level of the tree, dirs first, ignored ones marked. */
export function listDir(root: string, rel: string): TreeNode[] {
  let names: string[] = [];
  try {
    names = readdirSync(join(root, rel));
  } catch {
    return [];
  }
  const nodes: TreeNode[] = names.map((name) => {
    const path = rel ? `${rel}/${name}` : name;
    let dir = false;
    try {
      dir = statSync(join(root, path)).isDirectory();
    } catch {}
    return { name, path, dir, ignored: IGNORED.has(name) };
  });
  return nodes.sort((a, b) => Number(b.dir) - Number(a.dir) || a.name.localeCompare(b.name));
}

export type Match = { path: string; line: number; text: string };

/**
 * Content search across the working tree (tracked + untracked, binaries
 * skipped). Falls back to nothing outside a git repo — good enough while the
 * files view is git-backed anyway.
 */
export function grep(root: string, query: string, limit = 300): Match[] {
  if (!query.trim() || !isGitRepo(root)) return [];
  const out = run(root, [
    "grep", "-n", "-I", "--untracked", "--no-color",
    "--fixed-strings", "--ignore-case", "-e", query,
  ]);
  const matches: Match[] = [];
  for (const raw of out.split("\n")) {
    if (!raw) continue;
    // path:line:text — paths can contain ':' only rarely; split on the first two
    const first = raw.indexOf(":");
    const second = raw.indexOf(":", first + 1);
    if (first < 0 || second < 0) continue;
    const line = Number(raw.slice(first + 1, second));
    if (!line) continue;
    matches.push({ path: raw.slice(0, first), line, text: raw.slice(second + 1).trim() });
    if (matches.length >= limit) break;
  }
  return matches;
}
