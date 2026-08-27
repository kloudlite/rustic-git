import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { activity } from "@/lib/api";
import { ActivityFeed } from "@/components/app/activity-feed";
import { BackLink } from "@/components/repo/back-link";

/** Everything that has happened across this namespace, as far back as the feed
 *  goes. Deeper than the rail — more repos, more commits from each — but still
 *  not an archive: nothing keeps an event log, so this is what the directory and
 *  git can be asked for in one page load. */
export default async function ActivityPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  const events = await activity(token, owner, 100);
  if (!events.ok) {
    if (events.kind === "unauthorized") redirect("/login?from=expired");
    if (events.kind === "notFound") notFound();
    throw new Error(events.message);
  }

  return (
    <section className="mx-auto max-w-2xl">
      <BackLink href={`/${owner}/repos`}>Code Repos</BackLink>
      <h1 className="mt-3 text-title font-semibold tracking-title">Activity</h1>
      <p className="mt-2 text-sm2 text-muted-foreground">
        Commits, changes and repositories across {owner}.
      </p>
      <ActivityFeed events={events.value} />
    </section>
  );
}
