import Link from "next/link";
import { ArrowRight, SquareCode, Users } from "lucide-react";
import { RecentActivity } from "@/components/app/recent-activity";
import { ViewAs } from "@/components/app/view-as";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import { when } from "@/lib/time";
import type { ApiEnvironment, ApiEvent, ApiRepo, ApiWorkspace } from "@/lib/api";

/** What to pick up first: the things that are up, then alphabetical. Without an
 *  order the lists arrive personal-first, so somebody with six personal
 *  workspaces would never see a team's at all. */
function byUsefulness<T extends { state: string; name: string }>(a: T, b: T) {
  const up = (t: T) => (t.state === "running" || t.state === "ready" ? 0 : 1);
  return up(a) - up(b) || a.name.localeCompare(b.name);
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

/** The caption every block on this page is headed with. Only the blocks use it —
 *  the day labels inside the feed are deliberately quieter, so the eye counts
 *  three sections rather than three plus however many days the feed spans. */
function Caption({ children }: { children: React.ReactNode }) {
  return (
    <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
      {children}
    </h2>
  );
}

function More({ href, cta }: { href: string; cta: string }) {
  return (
    <Link
      href={href}
      className="inline-flex items-center gap-1 text-caption font-medium text-primary underline-offset-4 hover:underline"
    >
      {cta} <ArrowRight className="size-3" />
    </Link>
  );
}

function SectionHead({ title, href, cta }: { title: string; href: string; cta: string }) {
  return (
    <div className="flex items-baseline justify-between">
      <Caption>{title}</Caption>
      <More href={href} cta={cta} />
    </div>
  );
}

/** Home is one namespace's cockpit — a team's or a person's own handle, the same
 *  shape either way: the work that can be picked up right now, then what has
 *  happened in it, with the repos in the rail so cause and effect share a screen.
 *  The feed grows in place (`RecentActivity`); a landing page is not the
 *  place to slice by event kind. */
export function Home({
  owner,
  title,
  subtitle,
  canSwitch,
  members,
  repos,
  workspaces,
  environments,
  events,
}: {
  owner: string;
  title: string;
  subtitle: string;
  /** A team has a public half to switch to; a person's own handle has none. */
  canSwitch: boolean;
  /** Set only for a team — it is what the rail's Team block counts. */
  members?: number;
  repos: ApiRepo[];
  workspaces: ApiWorkspace[];
  environments: ApiEnvironment[];
  events: ApiEvent[];
}) {
  return (
    <>
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="truncate text-title font-semibold tracking-title">{title}</h1>
          <p className="mt-1 text-sm2 text-muted-foreground">{subtitle}</p>
        </div>
        {canSwitch && (
          <div className="flex h-8 shrink-0 items-center">
            <ViewAs slug={owner} view="member" />
          </div>
        )}
      </div>

      <div className="mt-8 grid gap-10 xl:grid-cols-overview">
        <section className="min-w-0">
          <div>
            <SectionHead title="Workspaces" href={`/${owner}/workspaces`} cta="All workspaces" />
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
                    owner={w.team || owner}
                    href={`/${w.team || owner}/workspaces`}
                    badge={<WsEnvStateBadge state={w.state} />}
                  />
                ))}
              </ul>
            )}
          </div>

          <div className="mt-8">
            <Caption>Recent activity</Caption>
            <RecentActivity owner={owner} initial={events} step={30} />
          </div>
        </section>

        <aside className="grid content-start gap-8">
          <section>
            <SectionHead title="Environments" href={`/${owner}/environments`} cta="All environments" />
            {environments.length === 0 ? (
              <Empty>No environments yet.</Empty>
            ) : (
              <ul className="mt-3 divide-y divide-border border border-border bg-card">
                {[...environments].sort(byUsefulness).slice(0, 5).map((e) => (
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
          </section>
          <section>
            <div className="flex items-baseline justify-between">
              <Caption>Repos</Caption>
              <More href={`/${owner}/repos`} cta="All repos" />
            </div>
            {repos.length === 0 ? (
              <Empty>No repositories yet.</Empty>
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
                      {typeof r.createdAt === "number" && (
                        <span className="shrink-0 text-caption text-muted-foreground">{when(r.createdAt)}</span>
                      )}
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </section>

          {/* A person's own handle has no members and nothing to configure, so the
              block is absent there rather than empty. */}
          {members !== undefined && (
            <section>
              <Caption>Team</Caption>
              <div className="mt-3 border border-border bg-card px-4 py-3">
                <p className="inline-flex items-center gap-1.5 text-sm2">
                  <Users className="size-4 shrink-0 text-muted-foreground" />
                  {members} {members === 1 ? "member" : "members"}
                </p>
                <Link
                  href={`/${owner}/settings`}
                  className="mt-2 block text-caption font-medium text-primary underline-offset-4 hover:underline"
                >
                  Team settings
                </Link>
              </div>
            </section>
          )}
        </aside>
      </div>
    </>
  );
}
