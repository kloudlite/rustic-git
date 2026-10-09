import { relative } from "node:path";
import { hello } from "./hello.ts";

export type { Change, ChangeStatus, Match, TreeNode } from "@kloudlite-tui/backend";

// The root is a path on the backend's machine (the bench, over the pipe), so shorten it against that
// machine's home and cwd from hello, never this process's.
export function displayRoot(root: string): string {
  const { home, cwd } = hello();
  return home && root.startsWith(home) ? `~${root.slice(home.length)}` : relative(cwd, root) || root;
}
