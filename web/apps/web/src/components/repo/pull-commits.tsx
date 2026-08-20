import Link from "next/link";
import { CopyButton } from "@/components/repo/copy-button";
import { commitTitle } from "@/components/repo/commit-meta";
import { shortOid } from "@/lib/browse";
import { whenSeconds } from "@/lib/time";
import type { ApiComparison } from "@/lib/api";

/** The PR's commits, oldest first — the order they will land in. */
export function PullCommits({
  owner,
  repo,
  comparison,
}: {
  owner: string;
  repo: string;
  comparison: ApiComparison | null;
}) {
  const base = `/${owner}/${repo}`;
  if (!comparison) {
    return (
      <p className="mt-6 border border-border bg-card px-4 py-10 text-center text-sm2 text-muted-foreground">
        The branches could not be read.
      </p>
    );
  }
  if (comparison.commits.length === 0) {
    return (
      <p className="mt-6 border border-border bg-card px-4 py-10 text-center text-sm2 text-muted-foreground">
        This branch adds nothing the base does not already have.
      </p>
    );
  }
  // Oldest first: the order they land in, which is not the order a log gives.
  const commits = [...comparison.commits].reverse();

  return (
    <ul className="mt-6 divide-y divide-border border border-border bg-card">
      {commits.map((c) => (
        <li key={c.oid} className="flex items-center gap-4 px-5 py-3.5">
          <div className="min-w-0 flex-1">
            <Link href={`${base}/commit/${c.oid}`} className="block truncate text-sm2 font-medium underline-offset-4 hover:underline">
              {commitTitle(c.message)}
            </Link>
            <p className="mt-1 text-caption text-muted-foreground">
              <span className="font-medium text-foreground/80">{c.author}</span> committed {whenSeconds(c.time)}
            </p>
          </div>
          <div className="flex shrink-0 items-center border border-edge">
            <Link href={`${base}/commit/${c.oid}`} className="px-2.5 py-1 font-mono text-caption text-primary hover:bg-muted">
              {shortOid(c.oid)}
            </Link>
            <CopyButton value={c.oid} label="Copy sha" className="border-l border-edge" />
          </div>
        </li>
      ))}
    </ul>
  );
}
