import type { Backend } from "@kloudlite-tui/backend";

/**
 * The reads Files draws from. A workspace view reads that workspace's pod (`podfs`) and ignores
 * `root` (a bench path); it never touches the bench-local `fs`. Grep has no pod endpoint.
 */
export function filesApi(workspace: string | undefined, be: Pick<Backend, "fs" | "podfs">): Backend["fs"] {
  if (!workspace) return be.fs;
  const p = be.podfs;
  return {
    isGitRepo: () => p.isGitRepo(workspace),
    changes: () => p.changes(workspace),
    fileDiff: (_r, path, status) => p.fileDiff(workspace, path, status),
    fullFile: (_r, path, status) => p.fullFile(workspace, path, status),
    listDir: (_r, rel) => p.listDir(workspace, rel),
    grep: () => Promise.reject(new Error("search is not available for workspaces")),
  };
}

/** The line shown when a workspace's pod cannot be read. */
export const unreachable = (e: unknown) => `workspace not reachable: ${e instanceof Error ? e.message : String(e)}`;
