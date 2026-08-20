import Link from "next/link";
import { Code2 } from "lucide-react";
import { CopyButton } from "@/components/repo/copy-button";
import { commitTitle } from "@/components/repo/commit-meta";
import { shortOid } from "@/lib/browse";
import { whenSeconds } from "@/lib/time";
import type { ApiComparison } from "@/lib/api";

/** The day a commit was made, in the reader's terms rather than an ISO string. */
const DAY = new Intl.DateTimeFormat("en", { month: "short", day: "numeric", year: "numeric" });

function Empty({ children }: { children: React.ReactNode }) {
  return (
    <p className="mt-6 border border-border bg-card px-4 py-10 text-center text-sm2 text-muted-foreground">
      {children}
    </p>
  );
}

/** The PR's commits, oldest first — the order they will land in — grouped by the
 *  day they were made, which is how a reviewer reads a branch: in sittings. */
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
  if (!comparison) return <Empty>The branches could not be read.</Empty>;
  if (comparison.commits.length === 0) {
    return <Empty>This branch adds nothing the base does not already have.</Empty>;
  }

  // Oldest first: the order they land in, which is the reverse of a log.
  const commits = [...comparison.commits].reverse();
  const days: { day: string; commits: typeof commits }[] = [];
  for (const c of commits) {
    const day = DAY.format(new Date(c.time * 1000));
    const last = days.at(-1);
    if (last && last.day === day) last.commits.push(c);
    else days.push({ day, commits: [c] });
  }

  return (
    <div className="mt-6 grid gap-6">
      {days.map(({ day, commits }) => (
        <section key={day}>
          <h2 className="text-sm2 text-muted-foreground">Commits on {day}</h2>
          <ul className="mt-2 divide-y divide-border border border-border bg-card">
            {commits.map((c) => (
              <li key={c.oid} className="flex items-center gap-4 px-5 py-3.5">
                <div className="min-w-0 flex-1">
                  <Link
                    href={`${base}/commit/${c.oid}`}
                    className="block truncate text-sm2 font-medium underline-offset-4 hover:underline"
                  >
                    {commitTitle(c.message)}
                  </Link>
                  <p className="mt-1 text-caption text-muted-foreground">
                    <span className="font-medium text-foreground/80">{c.author}</span> committed{" "}
                    {whenSeconds(c.time)}
                  </p>
                </div>
                <div className="flex shrink-0 items-center border border-edge">
                  <Link
                    href={`${base}/commit/${c.oid}`}
                    className="px-2.5 py-1 font-mono text-caption text-primary hover:bg-muted"
                  >
                    {shortOid(c.oid)}
                  </Link>
                  <CopyButton value={c.oid} label="Copy the full sha" className="border-l border-edge" />
                  {/* Browse the repo AT this commit — the reference's <> button. */}
                  <Link
                    href={`${base}/tree?ref=${encodeURIComponent(c.oid)}`}
                    aria-label="Browse the repository at this commit"
                    className="border-l border-edge px-2 py-1.5 text-muted-foreground hover:bg-muted hover:text-foreground"
                  >
                    <Code2 className="size-3.5" />
                  </Link>
                </div>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </div>
  );
}
