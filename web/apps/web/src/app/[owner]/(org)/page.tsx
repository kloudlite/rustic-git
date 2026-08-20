import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { activity, listRepos } from "@/lib/api";
import { Dashboard } from "@/components/app/dashboard";

/** An owner's Code Repos — their own handle or a team's, the same page either way.
 *
 *  Membership is not checked here: the api answers 404 for a namespace the caller
 *  may not act in, so asking it IS the check. Deciding locally would mean two
 *  places that know what a member is, and the browser-facing one would be guessing. */
export default async function OwnerPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  // Together: the feed is beside the list, and neither needs the other's answer.
  const [repos, events] = await Promise.all([
    listRepos(token, owner),
    activity(token, owner),
  ]);
  if (!repos.ok) {
    // An expired token is a session problem, not a missing namespace.
    if (repos.kind === "unauthorized") redirect("/login?from=expired");
    if (repos.kind === "notFound") notFound();
    throw new Error(repos.message);
  }

  // A feed that could not be read is an empty rail, not a broken page: the repo
  // list is what this page is for.
  return <Dashboard owner={owner} repos={repos.value} events={events.ok ? events.value : []} />;
}
