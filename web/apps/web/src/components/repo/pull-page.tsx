import Link from "next/link";
import { StateBadge } from "@/components/repo/pull-state";
import { cn } from "@/lib/utils";
import type { ApiPull } from "@/lib/api";

/** The header every PR view shares, and the level-3 tabs beneath it. These are
 *  tabs *inside the content*, not a third chrome row: they are about this PR.
 *
 *  The counts are real, which is why they are passed in — the conversation knows
 *  how many comments there are, and only a comparison against the base knows how
 *  many commits and files a change carries. */
export function PullHeader({
  owner,
  repo,
  pull,
  tab,
  counts,
}: {
  owner: string;
  repo: string;
  pull: ApiPull;
  tab: "conversation" | "commits" | "files";
  counts: { comments: number; commits: number | null; files: number | null };
}) {
  const base = `/${owner}/${repo}/pulls/${pull.number}`;
  const tabs = [
    { key: "conversation", href: base, label: "Conversation", count: counts.comments },
    { key: "commits", href: `${base}/commits`, label: "Commits", count: counts.commits },
    { key: "files", href: `${base}/files`, label: "Files changed", count: counts.files },
  ] as const;

  return (
    <header>
      <div className="flex flex-wrap items-start gap-x-3 gap-y-2">
        <h1 className="text-title font-semibold leading-tight tracking-title">
          {pull.title} <span className="font-normal text-muted-foreground">#{pull.number}</span>
        </h1>
      </div>
      <p className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-sm2 text-muted-foreground">
        <StateBadge state={pull.state} />
        <span>
          <span className="font-medium text-foreground/80">{pull.author}</span> wants to merge{" "}
          {counts.commits !== null && (
            <>{counts.commits} {counts.commits === 1 ? "commit" : "commits"} </>
          )}
          into{" "}
          <span className="border border-border bg-muted/40 px-1.5 font-mono text-caption text-foreground">{pull.base}</span>{" "}
          from{" "}
          <span className="border border-border bg-muted/40 px-1.5 font-mono text-caption text-foreground">{pull.head}</span>
        </span>
      </p>

      <nav className="mt-5 -mb-px flex gap-2 border-b border-border" aria-label="Pull request">
        {tabs.map((t) => (
          <Link
            key={t.key}
            href={t.href}
            aria-current={tab === t.key ? "page" : undefined}
            className={cn(
              "flex h-9 items-center gap-2 border-b-2 px-3 text-sm2 transition-colors",
              tab === t.key
                ? "border-primary font-medium text-foreground"
                : "border-transparent text-muted-foreground hover:border-border hover:text-foreground",
            )}
          >
            {t.label}
            {t.count !== null && (
              <span className={cn("px-1.5 text-micro font-medium", tab === t.key ? "bg-muted text-foreground" : "bg-muted/60 text-muted-foreground")}>
                {t.count}
              </span>
            )}
          </Link>
        ))}
      </nav>
    </header>
  );
}
