import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { listEnvironments, listVolumes, volumeHistory } from "@/lib/api";
import { EnvironmentList } from "@/components/app/environment-list";

export default async function Page({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

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

  // ARCHIVED rows: a volume on the server tier that still holds snapshots and has no live
  // Environment left. The snapshots outlive the object, so this is the only way back to them —
  // without it, deleting an environment made its own history unreachable.
  const live = new Set(list.value.map((e) => e.id));
  // Same `mine` rule as the environment list above: aggregate on the caller's own page, one
  // label on a team's.
  const volumes = await listVolumes(token, "environment", mine ? undefined : owner);
  const archivedRows = volumes.ok ? volumes.value.filter((v) => !live.has(v.name)) : [];
  // ponytail: one history read per archived volume, for the count. Archived rows are the deleted
  // ones, so the list is short; if it stops being short, the count belongs in `/v1/volumes`
  // itself, which needs a per-push marker under `index/` (see the server-tier handler).
  const archived = await Promise.all(
    archivedRows.map(async (v) => {
      const h = await volumeHistory(token, v.name);
      return {
        id: v.name,
        name: v.display_name,
        latest_ms: v.latest_ms,
        snapshots: h.ok ? h.value.length : 0,
        // `display_name` falls back to the volume id when no push recorded a name — the row says
        // so rather than passing an id off as one.
        named: v.display_name !== v.name,
      };
    }),
  );

  // The live rows' "snapshot 2 h ago". Same listing already read above, so this costs nothing:
  // a volume with no row has never been pushed and shows no time at all.
  const latest = Object.fromEntries(
    (volumes.ok ? volumes.value : []).map((v) => [v.name, v.latest_ms] as const),
  );

  return (
    <section>
      <EnvironmentList
        owner={owner}
        environments={list.value}
        archived={archived.filter((a) => a.snapshots > 0)}
        latest={latest}
      />
    </section>
  );
}
