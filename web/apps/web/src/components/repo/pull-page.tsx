import { FileDiff, GitCommitHorizontal, MessageSquare } from "lucide-react";
import { NavTabs } from "@/components/app/nav-tabs";
import { CopyButton } from "@/components/repo/copy-button";
import { StateBadge } from "@/components/repo/pull-state";
import { displayName } from "@/lib/person";
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
  counts,
  stat,
}: {
  owner: string;
  repo: string;
  pull: ApiPull;
  counts: { comments: number; commits: number | null; files: number | null };
  /** The whole change's line counts, shown against the tabs as on any forge. */
  stat?: { additions: number; deletions: number } | null;
}) {
  const base = `/${owner}/${repo}/pulls/${pull.number}`;
  // `exact` on Conversation only: its href is a prefix of the other two, so without
  // it /commits and /files would light it up as well.
  const tabs = [
    { href: base, label: "Conversation", count: counts.comments, icon: <MessageSquare />, exact: true },
    { href: `${base}/commits`, label: "Commits", count: counts.commits ?? undefined, icon: <GitCommitHorizontal /> },
    { href: `${base}/files`, label: "Files changed", count: counts.files ?? undefined, icon: <FileDiff /> },
  ];

  return (
    <header>
      <h1 className="text-title font-semibold leading-tight tracking-title">
        {pull.title} <span className="font-normal text-muted-foreground">#{pull.number}</span>
      </h1>
      <p className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-sm2 text-muted-foreground">
        <StateBadge state={pull.state} />
        <span>
          <span className="font-medium text-foreground/80">{displayName(pull.author)}</span> wants to merge{" "}
          {counts.commits !== null && (
            <>{counts.commits} {counts.commits === 1 ? "commit" : "commits"} </>
          )}
          into{" "}
          <span className="border border-border bg-muted px-1.5 font-mono text-caption text-foreground">{pull.base}</span>{" "}
          from{" "}
          <span className="border border-border bg-muted px-1.5 font-mono text-caption text-foreground">{pull.head}</span>
        </span>
        <CopyButton value={pull.head} label="Copy the branch name" />
      </p>

      {/* NavTabs has no right slot, so the stat rides beside it in the row that
          carries the border. */}
      <div className="mt-5 flex items-center border-b border-border">
        <NavTabs tabs={tabs} aria-label="Pull request" />
        {stat && (stat.additions > 0 || stat.deletions > 0) && (
          <span className="ml-auto font-mono text-caption">
            <span className="font-medium text-success">+{stat.additions.toLocaleString("en")}</span>{" "}
            <span className="font-medium text-destructive">−{stat.deletions.toLocaleString("en")}</span>
          </span>
        )}
      </div>
    </header>
  );
}
