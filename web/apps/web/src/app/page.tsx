import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { activity, listRepos } from "@/lib/api";
import { Landing } from "@/components/marketing/landing";
import { Home } from "@/components/app/home";

/** One route, two audiences. A signed-out visitor is being introduced to the
 *  product; a signed-in one gets the team feed — what changed since they last looked. */
export default async function HomePage({
  searchParams,
}: {
  searchParams: Promise<{ kind?: string }>;
}) {
  const session = await getSession();
  if (!session) return <Landing />;
  /* Everything past here builds URLs from the handle, so it has to exist first. */
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");
  const owner = session.user.owner;
  const { kind } = await searchParams;

  // Together: the feed and the rail are beside each other and neither needs the
  // other's answer. A feed that could not be read is an empty feed, not a broken
  // home page.
  const [events, repos] = await Promise.all([
    activity(token, owner, 50),
    listRepos(token, owner),
  ]);

  return (
    <Home
      session={session}
      events={events.ok ? events.value : []}
      repos={repos.ok ? repos.value : []}
      kind={kind === "commit" || kind === "pull" ? kind : ""}
    />
  );
}
