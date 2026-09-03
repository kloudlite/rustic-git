import { notFound, redirect } from "next/navigation";
import { listVolumes, listWorkspaces } from "@/lib/api";
import { archivedRows } from "@/lib/archived";
import { WorkspaceList } from "@/components/app/workspace-list";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { requireToken } from "@/lib/session";

/** Same guard shape as the repos org page: identity here, access left to the api. */
export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const { session, token } = await requireToken(`/${owner}/workspaces`);

  // The URL's owner is the team when it is not the person themselves; the api decides
  // membership and answers 404 for a team they are not in.
  //
  // The Snapshots section, the same one the environments page carries and for the same reason: a
  // volume whose Workspace is gone and whose snapshots are not. Deleting a workspace keeps them,
  // so this is the only way back to them — and the only place they can be deleted for good.
  // A failed read leaves the section empty rather than failing the page: the working set above is
  // what someone came here for.
  //
  // Neither read depends on the other; serial, they cost two 5 s timeouts instead of one.
  const scope = owner === session.user.owner ? undefined : owner;
  const [list, volumes] = await Promise.all([
    listWorkspaces(token, scope),
    listVolumes(token, "workspace", scope),
  ]);
  if (!list.ok) {
    if (list.kind === "unauthorized") redirect("/login?from=expired");
    if (list.kind === "notFound") notFound();
    throw new Error(list.message);
  }

  const rows = volumes.ok ? volumes.value : [];
  // The same listing, read once: a live workspace's Delete dialog names how many snapshots it
  // keeps rather than promising vaguely that some survive.
  const snapshots = Object.fromEntries(rows.map((v) => [v.name, v.snapshots] as const));

  return (
    <section>
      <AutoRefresh />
      <WorkspaceList
        owner={owner}
        workspaces={list.value}
        archived={archivedRows(rows)}
        snapshots={snapshots}
      />
    </section>
  );
}
