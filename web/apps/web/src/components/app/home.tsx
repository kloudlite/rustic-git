import Link from "next/link";
import { ArrowRight, CircleCheck, CircleX, GitCommitHorizontal, Layers, Rocket, Settings2, SquareCode, SquareTerminal, Tag } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { ENVIRONMENTS, FEED, REPOS, type FeedEvent } from "@/lib/mock";
import type { Session } from "@/lib/session";
import { Initials } from "@/components/app/initials";

const DAYS: FeedEvent["day"][] = ["Today", "Yesterday", "Earlier this week"];

/** The left column of a feed item: who or what did it. A person gets initials; a
 *  system event gets the icon of its kind, tinted only when it carries an outcome. */
function Origin({ event }: { event: FeedEvent }) {
  if (event.actor) {
    return <Initials name={event.actor.name} size={6} className="shrink-0" />;
  }
  const tone =
    event.ok === true ? "text-success" : event.ok === false ? "text-destructive" : "text-muted-foreground";
  const Icon =
    event.kind === "deploy" ? Rocket
    : event.kind === "pipeline" ? (event.ok === false ? CircleX : CircleCheck)
    : event.kind === "release" ? Tag
    : event.kind === "workspace" ? SquareTerminal
    : event.kind === "environment" ? Layers
    : GitCommitHorizontal;
  return (
    <span className="flex size-6 shrink-0 items-center justify-center">
      <Icon className={`size-4 ${tone}`} />
    </span>
  );
}

function FeedItem({ event, owner }: { event: FeedEvent; owner: string }) {
  return (
    <li className="flex gap-4 px-5 py-4">
      <Origin event={event} />
      <div className="min-w-0 flex-1">
        <p className="text-sm2 leading-snug">
          {event.actor && <span className="font-medium">{event.actor.login} </span>}
          {event.title}
          <span className="text-muted-foreground"> in </span>
          <Link href={`/${owner}/${event.repo}`} className="font-medium underline-offset-4 hover:underline">
            {event.repo}
          </Link>
          {event.ref && (
            <>
              <span className="text-muted-foreground"> · </span>
              <span className="font-mono text-caption text-muted-foreground">{event.ref}</span>
            </>
          )}
        </p>

        {event.commits && (
          <ul className="mt-2 grid gap-1.5">
            {event.commits.map((c) => (
              <li key={c.sha} className="flex items-baseline gap-2.5 text-caption">
                <span className="shrink-0 font-mono text-primary">{c.sha}</span>
                <span className="truncate text-muted-foreground">{c.message}</span>
              </li>
            ))}
          </ul>
        )}

        {event.detail && (
          <p className="mt-1.5 text-caption text-muted-foreground">{event.detail}</p>
        )}
      </div>
      <span className="shrink-0 text-caption text-muted-foreground">{event.when}</span>
    </li>
  );
}

/** Home is the team's feed: what happened across every repo, environment and
 *  workspace, newest first, grouped by day. The rail carries the current state
 *  the feed is changing — repos and environments — so cause and effect share a
 *  screen. */
export function Home({ session }: { session: NonNullable<Session> }) {
  return (
    <AppShell session={session} active="Home">
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <div className="grid gap-10 xl:grid-cols-overview">
          <section>
            <div className="flex flex-wrap items-center justify-between gap-3">
              <h1 className="text-title font-semibold tracking-title">
                What&rsquo;s happening in {session.user.owner}&rsquo;s team
              </h1>
              <Tabs defaultValue="all">
                <TabsList>
                  <TabsTrigger value="all">All</TabsTrigger>
                  <TabsTrigger value="pushes">Pushes</TabsTrigger>
                  <TabsTrigger value="deploys">Deploys</TabsTrigger>
                  <TabsTrigger value="pipelines">Pipelines</TabsTrigger>
                </TabsList>
              </Tabs>
            </div>

            <div className="mt-6 grid gap-8">
              {DAYS.map((day) => {
                const events = FEED.filter((e) => e.day === day);
                if (events.length === 0) return null;
                return (
                  <div key={day}>
                    <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
                      {day}
                    </h2>
                    <ul className="mt-3 divide-y divide-border border border-border">
                      {events.map((e) => <FeedItem key={e.id} event={e} owner={session.user.owner} />)}
                    </ul>
                  </div>
                );
              })}
            </div>
          </section>

          <aside className="grid content-start gap-8">
            <section>
              <div className="flex items-baseline justify-between">
                <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
                  Repos
                </h2>
                <Link
                  href={`/${session.user.owner}`}
                  className="inline-flex items-center gap-1 text-caption font-medium text-primary underline-offset-4 hover:underline"
                >
                  All repos <ArrowRight className="size-3" />
                </Link>
              </div>
              <ul className="mt-3 divide-y divide-border border border-border">
                {REPOS.map((r) => (
                  <li key={r.name}>
                    <Link href={`/${session.user.owner}/${r.name}`} className="flex items-center gap-3 px-4 py-3 transition-colors hover:bg-muted/60">
                      {r.system
                        ? <Settings2 className="size-4 shrink-0 text-primary" />
                        : <SquareCode className="size-4 shrink-0 text-muted-foreground" />}
                      <span className="min-w-0 flex-1 truncate text-sm2 font-medium">{r.name}</span>
                      <span className="text-caption text-muted-foreground">{r.updated}</span>
                      {r.pipeline === "failing"
                        ? <CircleX className="size-4 text-destructive" aria-label="Pipeline failing" />
                        : r.pipeline === "passing"
                          ? <CircleCheck className="size-4 text-success" aria-label="Pipeline passing" />
                          : null}
                    </Link>
                  </li>
                ))}
              </ul>
            </section>

            <section>
              <div className="flex items-baseline justify-between">
                <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
                  Environments
                </h2>
                <Link
                  href={`/${session.user.owner}/environments`}
                  className="inline-flex items-center gap-1 text-caption font-medium text-primary underline-offset-4 hover:underline"
                >
                  All <ArrowRight className="size-3" />
                </Link>
              </div>
              <ul className="mt-3 divide-y divide-border border border-border">
                {ENVIRONMENTS.map((e) => (
                  <li key={e.name} className="flex items-center gap-3 px-4 py-3">
                    <span className={`size-1.5 shrink-0 ${e.healthy ? "bg-success" : "bg-destructive"}`} aria-hidden />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm2 font-medium">{e.name}</div>
                      <div className="truncate text-caption text-muted-foreground">{e.repo}</div>
                    </div>
                    <span className="font-mono text-caption text-muted-foreground">{e.sha}</span>
                  </li>
                ))}
              </ul>
            </section>
          </aside>
        </div>
      </main>
    </AppShell>
  );
}
