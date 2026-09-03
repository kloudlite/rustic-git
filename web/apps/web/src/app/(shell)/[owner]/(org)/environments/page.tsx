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
  const mine = owner === session.user.username;
  const list = await listEnvironments(token, mine ? undefined : owner);
  if (!list.ok) {
    if (list.kind === "unauthorized") redirect("/login?from=expired");
    if (list.kind === "notFound") notFound();
    throw new Error(list.message);
  }

  // The Snapshots section: a volume whose Environment is gone and whose snapshots are not. The
  // snapshots outlive the object and are the only thing keeping the volume, so this is the way
  // back to them — without it, deleting an environment made its own history unreachable.
  // Same `mine` rule as the environment list above: aggregate on the caller's own page, one
  // label on a team's.
  const volumes = await listVolumes(token, "environment", mine ? undefined : owner);
  const archived = archivedRows(volumes.ok ? volumes.value : []);

  // The live rows' "last push 2 h ago". Same listing already read above, so this costs nothing:
  // a volume with no row has never been pushed and shows no time at all. `last_push_at`, never
  // `latest_ms` — that one counts sync points, which are internal and never shown.
  const latest = Object.fromEntries(
    (volumes.ok ? volumes.value : []).map((v) => [v.name, v.last_push_at] as const),
  );

  return (
    <section>
      <AutoRefresh />
      <EnvironmentList
        owner={owner}
        environments={list.value}
        archived={archived}
        latest={latest}
      />
    </section>
  );
}
