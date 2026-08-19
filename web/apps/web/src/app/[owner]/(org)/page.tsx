import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { listRepos } from "@/lib/api";
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

  const repos = await listRepos(token, owner);
  if (!repos.ok) {
    if (repos.kind === "notFound" || repos.kind === "unauthorized") notFound();
    throw new Error(repos.message);
  }

  return <Dashboard owner={owner} repos={repos.value} />;
}
