import { notFound, redirect } from "next/navigation";
import { listEnvironments, listVolumes } from "@/lib/api";
import { archivedRows } from "@/lib/archived";
import { EnvironmentList } from "@/components/app/environment-list";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { requireToken } from "@/lib/session";

export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const { session, token } = await requireToken(`/${owner}/environments`);

  // On the caller's OWN page, no owner filter: the api then aggregates personal envs plus
  // every team the caller belongs to — environments are a team-wide view. A team's page
  // keeps the filter so it shows exactly that team's.
  //
  // The Snapshots section: a volume whose Environment is gone and whose snapshots are not. The
  // snapshots outlive the object and are the only thing keeping the volume, so this is the way
  // back to them — without it, deleting an environment made its own history unreachable.
  // Same `mine` rule as the environment list above: aggregate on the caller's own page, one
  // label on a team's.
  const mine = owner === session.user.username;
  const [list, volumes] = await Promise.all([
    listEnvironments(token, mine ? undefined : owner),
    listVolumes(token, "environment", mine ? undefined : owner),
  ]);
  if (!list.ok) {
    if (list.kind === "unauthorized") redirect("/login?from=expired");
    if (list.kind === "notFound") notFound();
    throw new Error(list.message);
  }
  const rows = volumes.ok ? volumes.value : [];

  // Two maps off the ONE listing, both keyed by volume id, exactly as the workspaces page does
  // it: the live rows' "last push 2 h ago", and the count each live row's Delete dialog names.
  // `last_push_at`, never `latest_ms` — that one counts sync points, which are internal and
  // never shown. A volume with no row here has never been pushed and shows no time at all.
  const latest = Object.fromEntries(rows.map((v) => [v.name, v.last_push_at] as const));
  const snapshots = Object.fromEntries(rows.map((v) => [v.name, v.snapshots] as const));

  return (
    <section>
      <AutoRefresh />
      <EnvironmentList
        owner={owner}
        environments={list.value}
        archived={archivedRows(rows)}
        latest={latest}
        snapshots={snapshots}
      />
    </section>
  );
}
