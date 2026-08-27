import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { activity, listRepos } from "@/lib/api";
import { RepoList } from "@/components/app/repo-list";
import { ActivityFeed } from "@/components/app/activity-feed";

/** An owner's repositories — their own handle or a team's, the same page either way.
 *
 *  Membership is not checked here: the api answers 404 for a namespace the caller
 *  may not act in, so asking it IS the check. There is no public half of this page —
 *  a stranger reads a team's repos off its profile at `/{owner}`. */
export default async function ReposPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");
  const token = await apiToken();
  if (!token) redirect("/login");

  // Together: the feed is decoration — only the repo list can fail the page.
  const [repos, events] = await Promise.all([listRepos(token, owner), activity(token, owner, 10)]);
  if (!repos.ok) {
    // An expired token is a session problem, not a missing namespace.
    if (repos.kind === "unauthorized") redirect("/login?from=expired");
    if (repos.kind === "notFound") notFound();
    throw new Error(repos.message);
  }

  return (
    <div className="grid gap-10 xl:grid-cols-overview">
      <section className="min-w-0">
        <RepoList owner={owner} repos={repos.value} />
      </section>

      <aside>
        <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
          Activity
        </h2>
        <ActivityFeed events={events.ok ? events.value : []} more={`/${owner}/activity`} />
      </aside>
    </div>
  );
}
