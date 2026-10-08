import { relative } from "node:path";

export type { Change, ChangeStatus, Match, TreeNode } from "@kloudlite-tui/backend";

export function displayRoot(root: string): string {
  const home = process.env.HOME ?? "";
  return home && root.startsWith(home) ? `~${root.slice(home.length)}` : relative(process.cwd(), root) || root;
}
