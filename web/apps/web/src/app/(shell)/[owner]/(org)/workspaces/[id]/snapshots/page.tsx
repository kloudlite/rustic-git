import Link from "next/link";
import { ArrowLeft, Camera } from "lucide-react";
import { notFound, redirect } from "next/navigation";
import { volumeHistory } from "@/lib/api";
import { when } from "@/lib/time";
import { snapshotTime } from "@/lib/snapshot";
import { stateSummary } from "@/lib/snapshot-state";
import { DeleteSnapshotDialog, RestoreDialog } from "@/components/app/restore-dialog";
import { requireToken } from "@/lib/session";

/** A workspace's OWN snapshots — the only user-facing surface for them, reached from that
 *  workspace's row and nowhere else. Environment snapshots are the shared artifact and live on
 *  the Snapshots tab; a workspace's are durability and undo for the person who owns it.
 *
 *  The api enforces that, not this page: `/v1/volumes/{id}/history` and `/v1/workspaces/restore`
 *  both scope to volumes under the caller's own owner label, so a teammate who guesses the id
 *  gets a 404. Restoring produces a NEW workspace from the chosen snapshot — restoring in place
 *  is deliberately not offered. */
export default async function Page({
  params,
}: {
  params: Promise<{ owner: string; id: string }>;
}) {
  const { owner, id } = await params;
  const { token } = await requireToken(`/${owner}/workspaces/${id}/snapshots`);

  const history = await volumeHistory(token, id);
  if (!history.ok) {
    if (history.kind === "unauthorized") redirect("/login?from=expired");
    if (history.kind === "notFound") notFound();
    throw new Error(history.message);
  }

  return (
    <section>
      <Link
        href={`/${owner}/workspaces`}
        className="flex items-center gap-1.5 text-sm2 text-muted-foreground transition-colors hover:text-foreground"
      >
        <ArrowLeft className="size-3.5" />Workspaces
      </Link>
      <h1 className="mt-2 flex items-center gap-2 text-body font-medium">
        <Camera className="size-4 text-muted-foreground" aria-hidden />
        {id}
      </h1>

      {history.value.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          No snapshots yet. Push the workspace to take one.
        </p>
      ) : (
        <ul className="mt-5 divide-y divide-border border border-border bg-card">
          {history.value.map((c) => {
            const summary = stateSummary(c.state);
            return (
            <li key={c.id} className="flex flex-wrap items-center gap-4 px-5 py-4">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-3">
                  <span className="font-mono text-sm2">{c.id.slice(0, 8)}</span>
                  <span className={`truncate text-sm2 ${c.message ? "text-foreground" : "text-muted-foreground/50 italic"}`}>
                    {c.message || "—"}
                  </span>
                </div>
                {summary && <span className="mt-1 block text-sm2 text-muted-foreground">{summary}</span>}
                <span className="mt-1 block text-caption text-muted-foreground">
                  {when(snapshotTime(c))}
                </span>
              </div>
              <div className="flex shrink-0 items-center gap-1">
                <RestoreDialog owner={owner} snapshotId={c.id} state={c.state} />
                <DeleteSnapshotDialog
                  owner={owner}
                  id={id}
                  snapshotId={c.id}
                  label={c.message || c.id.slice(0, 8)}
                />
              </div>
            </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
