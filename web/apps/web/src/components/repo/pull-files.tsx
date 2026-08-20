import { FileCode } from "lucide-react";
import { DiffFiles } from "@/components/repo/diff-files";
import type { ParsedDiff } from "@/lib/diff";

/** Files changed: a jump list of files on the left, every diff on the right. */
export function PullFiles({ base, diff }: { base: string; diff: ParsedDiff | null }) {
  if (!diff || diff.files.length === 0) {
    return (
      <p className="mt-6 border border-border bg-card px-4 py-10 text-center text-sm2 text-muted-foreground">
        {diff ? "No files changed." : "The branches could not be read."}
      </p>
    );
  }

  return (
    <div className="mt-6 grid min-w-0 gap-8 lg:grid-cols-code">
      <aside className="hidden min-w-0 lg:block">
        <div className="sticky top-28">
          <p className="text-caption text-muted-foreground">
            {diff.files.length} {diff.files.length === 1 ? "file" : "files"} ·{" "}
            <span className="text-success">+{diff.additions}</span>{" "}
            <span className="text-destructive">−{diff.deletions}</span>
          </p>
          <ul className="mt-2 grid gap-px text-sm2">
            {diff.files.map((f) => (
              <li key={f.path}>
                <a href={`#${f.path}`} className="flex h-7 items-center gap-2 px-2 hover:bg-muted">
                  <FileCode className="size-4 shrink-0 text-muted-foreground" />
                  <span className="truncate font-mono text-caption">{f.path}</span>
                </a>
              </li>
            ))}
          </ul>
        </div>
      </aside>

      <div className="min-w-0">
        <DiffFiles diff={diff} base={base} />
      </div>
    </div>
  );
}
