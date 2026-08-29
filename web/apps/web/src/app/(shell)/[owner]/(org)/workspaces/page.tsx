import { notFound, redirect } from "next/navigation";
import { listWorkspaces } from "@/lib/api";
import { WorkspaceList } from "@/components/app/workspace-list";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { requireToken } from "@/lib/session";

/** Same guard shape as the repos org page: identity here, access left to the api. */
export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const { session, token } = await requireToken(`/${owner}/workspaces`);

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
      <AutoRefresh />
      <WorkspaceList owner={owner} workspaces={list.value} />
    </section>
  );
}
