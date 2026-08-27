import Link from "next/link";
import { ArrowLeft, Camera } from "lucide-react";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { volumeHistory } from "@/lib/api";
import { when } from "@/lib/time";
import { RestoreDialog } from "@/components/app/restore-dialog";

/** One row per snapshot, newest first — the api's own contract for `history`. `kind` comes
 *  from `volume-list.tsx`'s link query param: only a workspace's snapshots can be
 *  restored (`POST /v1/workspaces/restore`) — an environment has no such route. */
export default async function Page({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string; id: string }>;
  searchParams: Promise<{ kind?: string }>;
}) {
  const { owner, id } = await params;
  const { kind } = await searchParams;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  const history = await volumeHistory(token, id);
  if (!history.ok) {
    if (history.kind === "unauthorized") redirect("/login?from=expired");
    if (history.kind === "notFound") notFound();
    throw new Error(history.message);
  }

  return (
    <section>
      <Link
        href={`/${owner}/snapshots`}
        className="flex items-center gap-1.5 text-sm2 text-muted-foreground transition-colors hover:text-foreground"
      >
        <ArrowLeft className="size-3.5" />Snapshots
      </Link>
      <h1 className="mt-2 flex items-center gap-2 text-body font-medium">
        <Camera className="size-4 text-muted-foreground" aria-hidden />
        {id}
      </h1>

      {history.value.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          No snapshots yet.
        </p>
      ) : (
        <ul className="mt-5 divide-y divide-border border border-border bg-card">
          {history.value.map((c) => (
            <li key={c.id} className="flex flex-wrap items-center gap-4 px-5 py-4">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-3">
                  <span className="font-mono text-sm2">{c.id.slice(0, 8)}</span>
                  <span className={`truncate text-sm2 ${c.message ? "text-foreground" : "text-muted-foreground/50 italic"}`}>
                    {c.message || "—"}
                  </span>
                </div>
                {/* Layer counts and the tip sha are gone with the registry read: a snapshot's
                    lineage is layer bookkeeping that lives with the bytes on the server tier, and
                    copying it into a CR would put megabytes into an object the API server lists.
                    What a person picks a snapshot by — when, and what they called it — is here. */}
                <span className="mt-1 block text-caption text-muted-foreground">
                  {when(new Date(c.created_at).getTime())}
                </span>
              </div>
              {kind === "workspace" && <RestoreDialog owner={owner} srcWorkspace={id} snapshotId={c.id} />}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
