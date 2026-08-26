import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { listWorkspaces } from "@/lib/api";
import { WorkspaceList } from "@/components/app/workspace-list";

/** Same guard shape as the repos org page: identity here, access left to the api. */
export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  // The URL's owner is the team when it is not the person themselves; the api decides
  // membership and answers 404 for a team they are not in.
  const list = await listWorkspaces(token, owner === session.user.owner ? undefined : owner);
  if (!list.ok) {
    if (list.kind === "unauthorized") redirect("/login?from=expired");
    if (list.kind === "notFound") notFound();
    throw new Error(list.message);
  }

  return (
    <section>
      <WorkspaceList owner={owner} workspaces={list.value} />
    </section>
  );
}
