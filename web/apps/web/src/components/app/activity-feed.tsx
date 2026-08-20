import Link from "next/link";
import { GitCommitHorizontal, GitMerge, GitPullRequest, FolderPlus } from "lucide-react";
import { whenSeconds } from "@/lib/time";
import type { ApiEvent } from "@/lib/api";

const ICON = {
  commit: GitCommitHorizontal,
  pull_opened: GitPullRequest,
  pull_merged: GitMerge,
  repo_created: FolderPlus,
} as const;

/** What has happened lately, newest first.
 *
 *  Every row is something that actually happened: a commit that is in the repo, a
 *  change somebody opened, a repo that exists. There are no deploys or pipeline
 *  runs here because nothing in this system runs one — a feed that invents them
 *  is worse than a short feed, since a reader cannot tell which rows to trust. */
export function ActivityFeed({ events }: { events: ApiEvent[] }) {
  if (events.length === 0) {
    return (
      <div className="mt-4 border border-border bg-card px-4 py-10 text-center">
        <p className="text-sm2 font-medium">Nothing yet</p>
        <p className="mx-auto mt-1 max-w-56 text-caption text-muted-foreground">
          Pushes, changes and new repositories show up here.
        </p>
      </div>
    );
  }

  return (
    <ul className="mt-4 divide-y divide-border border border-border bg-card">
      {events.map((e, i) => {
        const Icon = ICON[e.kind] ?? GitCommitHorizontal;
        return (
          <li key={`${e.href}-${e.kind}-${i}`}>
            <Link href={e.href} className="flex items-start gap-3 px-4 py-3.5 transition-colors hover:bg-muted/50">
              <Icon className={`mt-0.5 size-4 shrink-0 ${e.kind === "pull_merged" ? "text-primary" : "text-muted-foreground"}`} />
              <div className="min-w-0 flex-1">
                <p className="truncate text-sm2 leading-snug">
                  {e.actor && <span className="font-medium">{actorName(e.actor)}</span>} {e.title}
                </p>
                <p className="mt-1 flex items-center gap-1.5 text-caption text-muted-foreground">
                  <span className="truncate">{e.repo}</span>
                  {e.detail && (
                    <>
                      <span aria-hidden>·</span>
                      <span className="truncate font-mono">{e.detail}</span>
                    </>
                  )}
                </p>
              </div>
              <span className="shrink-0 text-caption text-muted-foreground">{whenSeconds(e.at)}</span>
            </Link>
          </li>
        );
      })}
    </ul>
  );
}

/** An email is how the directory names a person; a feed is not the place to
 *  print one in full. */
function actorName(actor: string) {
  return actor.includes("@") ? actor.split("@")[0] : actor;
}
