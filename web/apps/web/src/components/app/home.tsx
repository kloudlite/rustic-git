import Link from "next/link";
import { ArrowRight, GitCommitHorizontal, GitPullRequest, Rows3, SquareCode } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { NavTabs } from "@/components/app/nav-tabs";
import { ActivityFeed } from "@/components/app/activity-feed";
import { when } from "@/lib/time";
import type { ApiEvent, ApiRepo } from "@/lib/api";
import type { Session } from "@/lib/session";

/** The feed's filters.
 *
 *  The same tab row as every other level of navigation, so they look and behave
 *  alike — one underline that slides rather than four that blink. The filter is a
 *  query parameter, which is why the row is told what is active: every filter
 *  shares one path, so reading the URL would highlight nothing. */
const FILTERS = [
  { key: "", label: "All", icon: <Rows3 /> },
  { key: "commit", label: "Commits", icon: <GitCommitHorizontal /> },
  { key: "pull", label: "Changes", icon: <GitPullRequest /> },
] as const;

function group(events: ApiEvent[]) {
  const now = Date.now() / 1000;
  const day = 24 * 60 * 60;
  const buckets: { label: string; events: ApiEvent[] }[] = [
    { label: "Today", events: [] },
    { label: "Yesterday", events: [] },
    { label: "Earlier", events: [] },
  ];
  for (const e of events) {
    const age = now - e.at;
    buckets[age < day ? 0 : age < 2 * day ? 1 : 2].events.push(e);
  }
  return buckets.filter((b) => b.events.length > 0);
}

/** Home is the team's feed: what happened across every repo, newest first,
 *  grouped by day. The rail carries the repos the feed is about, so cause and
 *  effect share a screen. */
export function Home({
  session,
  events,
  repos,
  kind,
}: {
  session: NonNullable<Session>;
  events: ApiEvent[];
  repos: ApiRepo[];
  kind: string;
}) {
  const owner = session.user.owner;
  const shown = kind ? events.filter((e) => e.kind.startsWith(kind)) : events;
  const days = group(shown);

  return (
    <AppShell session={session}>
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <div className="grid gap-10 xl:grid-cols-overview">
          <section className="min-w-0">
            <h1 className="text-title font-semibold tracking-title">
              What&rsquo;s happening in {owner}&rsquo;s team
            </h1>
            <NavTabs
              tabs={FILTERS.map((f) => ({
                href: f.key ? `/?kind=${f.key}` : "/",
                label: f.label,
                icon: f.icon,
              }))}
              activeHref={kind ? `/?kind=${kind}` : "/"}
              className="mt-4 border-b border-border"
              aria-label="Filter the feed"
            />

            {days.length === 0 ? (
              <div className="mt-6 border border-border bg-card px-4 py-14 text-center">
                <p className="text-sm2 font-medium">Nothing here yet</p>
                <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
                  Push a commit or open a change and it will show up here.
                </p>
              </div>
            ) : (
              <div className="mt-6 grid gap-8">
                {days.map((d) => (
                  <div key={d.label}>
                    <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
                      {d.label}
                    </h2>
                    <ActivityFeed events={d.events} />
                  </div>
                ))}
              </div>
            )}
          </section>

          <aside className="grid content-start gap-8">
            <section>
              <div className="flex items-baseline justify-between">
                <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
                  Repos
                </h2>
                <Link
                  href={`/${owner}`}
                  className="inline-flex items-center gap-1 text-caption font-medium text-primary underline-offset-4 hover:underline"
                >
                  All repos <ArrowRight className="size-3" />
                </Link>
              </div>
              {repos.length === 0 ? (
                <p className="mt-3 border border-border bg-card px-4 py-8 text-center text-caption text-muted-foreground">
                  No repositories yet.
                </p>
              ) : (
                <ul className="mt-3 divide-y divide-border border border-border bg-card">
                  {repos.slice(0, 8).map((r) => (
                    <li key={r._id}>
                      <Link
                        href={`/${r.owner}/${r.name}`}
                        className="flex items-center gap-3 px-4 py-3 transition-colors hover:bg-muted/60"
                      >
                        <SquareCode className="size-4 shrink-0 text-muted-foreground" />
                        <span className="min-w-0 flex-1 truncate text-sm2 font-medium">{r.name}</span>
                        <span className="shrink-0 text-caption text-muted-foreground">
                          {when(r.createdAt)}
                        </span>
                      </Link>
                    </li>
                  ))}
                </ul>
              )}
            </section>
          </aside>
        </div>
      </main>
    </AppShell>
  );
}
