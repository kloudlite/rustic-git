import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { activity, listEnvironments, listTeams, listWorkspaces } from "@/lib/api";
import { Landing } from "@/components/marketing/landing";
import { Home, mergeFeeds, type HomeOwner } from "@/components/app/home";

/** One route, two audiences. A signed-out visitor is being introduced to the
 *  product; a signed-in one gets their own cockpit — the work they can pick up
 *  and what happened across every owner they belong to. */
export default async function HomePage() {
  const session = await getSession();
  if (!session) return <Landing />;
  /* Everything past here builds URLs from the handle, so it has to exist first. */
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  // `ownersFor` drops the member counts the rail wants, so read the teams here
  // and derive the owners the same way it does — keep in step with `lib/owners.ts`. A team list that fails to load
  // leaves the person with their own namespace, never an error page.
  const teams = await listTeams(token);
  const owners: HomeOwner[] = [
    { slug: session.user.owner, name: session.user.name, personal: true },
    ...(teams.ok ? teams.value.map((t) => ({ slug: t._id, name: t.name, members: t.members.length })) : []),
  ];

  // Together: none of these needs another's answer, and one that could not be
  // read is an empty section, not a broken home page. Workspaces are per-owner
  // on the wire; environments and the feed are not.
  // ponytail: 2 + teams + owners requests per render, which is fine at a handful of
  // teams and quadratic-feeling at fifty; collapse to a multi-owner activity endpoint
  // and an `?all=1` on /v1/workspaces when anyone feels it.
  const [ws, envs, feeds] = await Promise.all([
    Promise.all([listWorkspaces(token), ...owners.filter((o) => !o.personal).map((o) => listWorkspaces(token, o.slug))]),
    listEnvironments(token),
    Promise.all(owners.map((o) => activity(token, o.slug, 20))),
  ]);

  return (
    <Home
      session={session}
      owners={owners}
      workspaces={ws.flatMap((r) => (r.ok ? r.value : []))}
      environments={envs.ok ? envs.value : []}
      events={mergeFeeds(feeds.map((f) => (f.ok ? f.value : [])), 30)}
    />
  );
}
