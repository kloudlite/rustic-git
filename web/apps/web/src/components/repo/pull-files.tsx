import { ChevronDown, FileCode, Folder } from "lucide-react";
import { DiffFiles } from "@/components/repo/diff-files";
import type { ParsedDiff } from "@/lib/diff";
import { pathHref } from "@/lib/utils";

type Node = { name: string; path: string; children: Node[]; file: boolean };

/** The changed paths as the directory tree they came from.
 *
 *  A flat list of 600 paths is not navigable; the tree is how a reviewer finds
 *  the file they care about. Built from the diff itself, so it contains exactly
 *  the files that changed and no directory that leads nowhere. */
function tree(paths: string[]): Node[] {
  const root: Node = { name: "", path: "", children: [], file: false };
  for (const p of paths) {
    let at = root;
    const parts = p.split("/");
    parts.forEach((part, i) => {
      const last = i === parts.length - 1;
      const path = parts.slice(0, i + 1).join("/");
      let next = at.children.find((c) => c.name === part && c.file === last);
      if (!next) {
        next = { name: part, path, children: [], file: last };
        at.children.push(next);
      }
      at = next;
    });
  }
  // Collapse a directory with a single directory inside it into `a/b`, the way
  // every file browser does — otherwise a deep path is a column of one-item rows.
  const squash = (n: Node): Node => {
    let node = n;
    while (!node.file && node.children.length === 1 && !node.children[0].file) {
      const only = node.children[0];
      node = { ...only, name: `${node.name}/${only.name}` };
    }
    return { ...node, children: node.children.map(squash) };
  };
  return root.children.map(squash);
}

function Branch({ nodes, depth = 0 }: { nodes: Node[]; depth?: number }) {
  return (
    <ul className={depth === 0 ? "grid gap-px" : "grid gap-px"}>
      {nodes.map((n) =>
        n.file ? (
          <li key={n.path}>
            <a
              href={`#${pathHref(n.path)}`}
              className="flex h-7 items-center gap-1.5 px-2 hover:bg-muted"
              style={{ paddingLeft: `${8 + depth * 12}px` }}
              title={n.path}
            >
              <FileCode className="size-3.5 shrink-0 text-muted-foreground" />
              <span className="truncate font-mono text-caption">{n.name}</span>
            </a>
          </li>
        ) : (
          <li key={n.path}>
            <div
              className="flex h-7 items-center gap-1.5 px-2 text-muted-foreground"
              style={{ paddingLeft: `${8 + depth * 12}px` }}
            >
              <ChevronDown className="size-3.5 shrink-0" />
              <Folder className="size-3.5 shrink-0" />
              <span className="truncate text-caption">{n.name}</span>
            </div>
            <Branch nodes={n.children} depth={depth + 1} />
          </li>
        ),
      )}
    </ul>
  );
}

/** Files changed: the tree of changed files on the left, every diff on the right. */
export function PullFiles({ base, diff }: { base: string; diff: ParsedDiff | null }) {
  if (!diff || diff.files.length === 0) {
    return (
      <p className="mt-6 border border-border bg-card px-4 py-10 text-center text-sm2 text-muted-foreground">
        {diff ? "No files changed." : "The branches could not be read."}
      </p>
    );
  }
  const nodes = tree(diff.files.map((f) => f.path));

  return (
    <div className="mt-6 grid min-w-0 gap-8 lg:grid-cols-code">
      <aside className="hidden min-w-0 lg:block">
        <div className="sticky top-28">
          <p className="px-2 text-caption text-muted-foreground">
            {diff.files.length} {diff.files.length === 1 ? "file" : "files"} changed
          </p>
          <div className="mt-2 max-h-[70vh] overflow-y-auto">
            <Branch nodes={nodes} />
          </div>
        </div>
      </aside>

      <div className="min-w-0">
        <DiffFiles diff={diff} base={base} />
      </div>
    </div>
  );
}
