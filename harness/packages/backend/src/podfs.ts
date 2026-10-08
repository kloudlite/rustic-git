//! A workspace's own files for the Files view: its pod's `~/workspace`, read from the pod's ide
//! server (crates/ide/src/fs: /fs/tree, /fs/changes, /fs/diff, /fs/file) through the platform, the
//! way the sidebar reads /fs/changes (space.ts). The backend makes the call, so a remote laptop TUI
//! gets it over the same wire as `fs`. Never falls back to the bench's own files.
import { podGet, podText } from "@kloudlite-tui/tools";
import type { Backend } from "./index.ts";
import type { Change, ChangeStatus, TreeNode } from "./git.ts";
import { parseUnified } from "./git.ts";

type PodEntry = { name: string; kind: string; ignored?: boolean };
type PodChange = { path: string; index: string; worktree: string; additions?: number; deletions?: number };

/** `/fs/tree` entries under `rel` as Files' tree nodes (dirs first is the pod's order). */
export function mapTree(rel: string, entries: PodEntry[]): TreeNode[] {
  const base = rel === "." || rel === "" ? "" : `${rel}/`;
  return entries.map((e) => ({ name: e.name, path: base + e.name, dir: e.kind === "dir", ignored: e.ignored || undefined }));
}

/** `/fs/changes` rows as Files' changes; untracked/added is A, any delete is D, the rest M. */
export function mapChanges(rows: PodChange[]): Change[] {
  return rows.map((c) => {
    const s = c.index + c.worktree;
    const status: ChangeStatus = s.includes("D") ? "D" : s.includes("?") || s.includes("A") ? "A" : "M";
    return { path: c.path, status, added: c.additions ?? 0, removed: c.deletions ?? 0 };
  });
}

const q = (o: Record<string, string>) => new URLSearchParams(o).toString();

const changes = async (ws: string) => mapChanges((await podGet<{ changes?: PodChange[] }>(ws, "/fs/changes")).changes ?? []);

export const podfs: Backend["podfs"] = {
  isGitRepo: async (ws) => (await podGet<{ repo?: boolean }>(ws, "/fs/git")).repo === true,
  changes,
  listDir: async (ws, rel) => mapTree(rel, (await podGet<{ entries?: PodEntry[] }>(ws, `/fs/tree?${q({ path: rel || "." })}`)).entries ?? []),
  fileDiff: async (ws, path) => {
    const r = await podGet<{ patch?: string }>(ws, `/fs/diff?${q({ path, against: "HEAD" })}`);
    if (!r.patch) return null;
    const lines = parseUnified(r.patch);
    return { path, lines, added: lines.filter((l) => l.sign === "+").length, removed: lines.filter((l) => l.sign === "-").length };
  },
  // ponytail: no per-line "new since HEAD" marks for a modified file (the bench reader runs a zero-context
  // diff for those); an added file is all green. Add from /fs/diff?against=HEAD hunks if it is missed.
  fullFile: async (ws, path, status) => {
    const text = await podText(ws, `/fs/file?${q({ path })}`);
    return text.split("\n").map((t, i) => ({ no: i + 1, sign: " " as const, text: t, mark: status === "A" ? ("added" as const) : undefined }));
  },
};
