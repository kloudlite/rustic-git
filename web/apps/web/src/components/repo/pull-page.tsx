import Link from "next/link";
import { GitPullRequest } from "lucide-react";
import { BackLink } from "@/components/repo/back-link";
import { PULL, REPO } from "@/lib/mock-repo";
import { cn } from "@/lib/utils";

/** The header every PR view shares, and the level-3 tabs beneath it. These are
 *  tabs *inside the content*, not a third chrome row: they are about this PR. */
export function PullHeader({ owner, tab }: { owner: string; tab: "conversation" | "commits" | "files" }) {
  const base = `/${owner}/${REPO.name}/pulls/${PULL.number}`;
  const tabs = [
    { key: "conversation", href: base, label: "Conversation", count: PULL.timeline.filter((t) => t.kind !== "checks").length },
    { key: "commits", href: `${base}/commits`, label: "Commits", count: PULL.commits.length },
    { key: "files", href: `${base}/files`, label: "Files changed", count: PULL.stats.files },
  ] as const;

  return (
    <header>
      <BackLink href={`/${owner}/${REPO.name}/pulls`}>Pull requests</BackLink>
      <div className="mt-3 flex flex-wrap items-start gap-x-3 gap-y-2">
        <h1 className="text-title font-semibold leading-tight tracking-title">
          {PULL.title} <span className="font-normal text-muted-foreground">#{PULL.number}</span>
        </h1>
      </div>
      <p className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-sm2 text-muted-foreground">
        <span className="inline-flex items-center gap-1.5 bg-success px-2 py-0.5 text-caption font-medium text-primary-foreground">
          <GitPullRequest className="size-3.5" /> Open
        </span>
        <span>
          <span className="font-medium text-foreground/80">{PULL.author}</span> wants to merge{" "}
          {PULL.commits.length} commits into{" "}
          <span className="border border-border bg-muted/40 px-1.5 font-mono text-caption text-foreground">{PULL.base}</span>{" "}
          from{" "}
          <span className="border border-border bg-muted/40 px-1.5 font-mono text-caption text-foreground">{PULL.head}</span>
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
            <span className={cn("px-1.5 text-micro font-medium", tab === t.key ? "bg-muted text-foreground" : "bg-muted/60 text-muted-foreground")}>{t.count}</span>
          </Link>
        ))}
      </nav>
    </header>
  );
}
