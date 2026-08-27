import Link from "next/link";
import { ArrowRight, Boxes, Plus, Users } from "lucide-react";
import { ActivityFeed } from "@/components/app/activity-feed";
import { Initials } from "@/components/app/initials";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import type { ApiEnvironment, ApiEvent, ApiWorkspace } from "@/lib/api";
import type { Owner } from "@/lib/owners";
import type { Session } from "@/lib/session";

/** An owner as the rail shows it: the member count only exists for teams. */
export type HomeOwner = Owner & { members?: number };

/** One feed out of many. Each owner's activity arrives separately and is already
 *  newest-first, but a plain concatenation would show one owner's week above
 *  another's hour — so interleave on the timestamp before capping. */
export function mergeFeeds(feeds: ApiEvent[][], limit: number): ApiEvent[] {
  return feeds.flat().sort((a, b) => b.at - a.at).slice(0, limit);
}

/** What to pick up first: the things that are up, then alphabetical. Without an
 *  order the lists arrive personal-first, so somebody with six personal
 *  workspaces would never see a team's at all. */
function byUsefulness<T extends { state: string; name: string }>(a: T, b: T) {
  const up = (t: T) => (t.state === "running" || t.state === "ready" ? 0 : 1);
  return up(a) - up(b) || a.name.localeCompare(b.name);
}

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

/** A compact row for a workspace or an environment: what it is, whose it is, how
 *  it is doing. The link goes to the owner's list rather than the thing itself —
 *  this page is a way back into work, and the list is where the actions live. */
function ThingRow({ name, owner, href, badge }: { name: string; owner: string; href: string; badge: React.ReactNode }) {
  return (
    <li>
      <Link
        href={href}
        className="flex items-center gap-3 px-4 py-3 transition-colors hover:bg-muted/60"
      >
        <span className="min-w-0 flex-1 truncate text-sm2 font-medium">{name}</span>
        <span className="shrink-0 border border-border px-1.5 py-0.5 text-caption text-muted-foreground">
          {owner}
        </span>
        {badge}
      </Link>
    </li>
  );
}

function Empty({ children }: { children: React.ReactNode }) {
  return (
    <p className="mt-3 border border-border bg-card px-4 py-8 text-center text-caption text-muted-foreground">
      {children}
    </p>
  );
}

function SectionHead({ title, href, cta }: { title: string; href: string; cta: string }) {
  return (
    <div className="flex items-baseline justify-between">
      <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
        {title}
      </h2>
      <Link
        href={href}
        className="inline-flex items-center gap-1 text-caption font-medium text-primary underline-offset-4 hover:underline"
      >
        {cta} <ArrowRight className="size-3" />
      </Link>
    </div>
  );
}

/** Home is the person's own cockpit: the work they can pick up right now —
 *  workspaces and environments across every team they are in — then what has
 *  happened across all of those owners, and the teams themselves in the rail.
 *  The feed's filters live on `/{owner}/activity`; a page about a person is not
 *  the place to slice by event kind. */
export function Home({
  session,
  owners,
  workspaces,
  environments,
  events,
}: {
  session: NonNullable<Session>;
  owners: HomeOwner[];
  workspaces: ApiWorkspace[];
  environments: ApiEnvironment[];
  events: ApiEvent[];
}) {
  const me = session.user.owner;
  const days = group(events);

  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <div className="grid gap-10 xl:grid-cols-overview">
        <section className="min-w-0">
          <h1 className="text-title font-semibold tracking-title">Home</h1>

          <div className="mt-8">
            <SectionHead title="Your workspaces" href={`/${me}/workspaces`} cta="All workspaces" />
            {workspaces.length === 0 ? (
              <Empty>No workspaces yet.</Empty>
            ) : (
              <ul className="mt-3 divide-y divide-border border border-border bg-card">
                {[...workspaces].sort(byUsefulness).slice(0, 6).map((w) => (
                  <ThingRow
                    key={w.id}
                    name={w.name}
                    /* `team` is empty for personal work, and that is the only
                       thing that tells the two namespaces apart on the wire. */
                    owner={w.team || me}
                    href={`/${w.team || me}/workspaces`}
                    badge={<WsEnvStateBadge state={w.state} />}
                  />
                ))}
              </ul>
            )}
          </div>

          <div className="mt-8">
            <SectionHead title="Your environments" href={`/${me}/environments`} cta="All environments" />
            {environments.length === 0 ? (
              <Empty>No environments yet.</Empty>
            ) : (
              <ul className="mt-3 divide-y divide-border border border-border bg-card">
                {[...environments].sort(byUsefulness).slice(0, 6).map((e) => (
                  <ThingRow
                    key={e.id}
                    name={e.name}
                    owner={e.owner}
                    href={`/${e.owner}/environments`}
                    badge={<WsEnvStateBadge state={e.state} />}
                  />
                ))}
              </ul>
            )}
          </div>

          <div className="mt-8">
            <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
              Recent activity
            </h2>
            {days.length === 0 ? (
              <div className="mt-3 border border-border bg-card px-4 py-14 text-center">
                <p className="text-sm2 font-medium">Nothing here yet</p>
                <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
                  Push a commit or open a change and it will show up here.
                </p>
              </div>
            ) : (
              <div className="mt-3 grid gap-8">
                {days.map((d) => (
                  <div key={d.label}>
                    <h3 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
                      {d.label}
                    </h3>
                    <ActivityFeed events={d.events} />
                  </div>
                ))}
              </div>
            )}
          </div>
        </section>

        <aside className="grid content-start gap-8">
          <section>
            <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
              Teams
            </h2>
            <ul className="mt-3 divide-y divide-border border border-border bg-card">
              {owners.map((o) => (
                <li key={o.slug}>
                  <Link
                    href={`/${o.slug}`}
                    className="flex items-center gap-3 px-4 py-3 transition-colors hover:bg-muted/60"
                  >
                    {o.personal ? (
                      <Initials name={o.name} tone="primary" />
                    ) : (
                      <Boxes className="size-4 shrink-0 text-muted-foreground" />
                    )}
                    <span className="min-w-0 flex-1 truncate text-sm2 font-medium">{o.name}</span>
                    {o.members !== undefined && (
                      <span className="inline-flex shrink-0 items-center gap-1 text-caption text-muted-foreground">
                        <Users className="size-3" />
                        {o.members}
                      </span>
                    )}
                  </Link>
                </li>
              ))}
              <li>
                <Link
                  href="/new-team"
                  className="flex items-center gap-3 px-4 py-3 text-primary transition-colors hover:bg-muted/60"
                >
                  <Plus className="size-4 shrink-0" />
                  <span className="text-sm2 font-medium">New team</span>
                </Link>
              </li>
            </ul>
          </section>
        </aside>
      </div>
    </main>
  );
}
