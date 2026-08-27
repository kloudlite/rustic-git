import Link from "next/link";
import { ArrowLeft, Camera } from "lucide-react";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { volumeHistory } from "@/lib/api";
import { when } from "@/lib/time";
import { RestoreEnvDialog } from "@/components/app/restore-dialog";

/** One environment's snapshots, opened from its row on the Environments page — live or archived.
 *
 *  Reads by volume id and needs no live Environment: the records live on the server tier and
 *  outlive the thing they were taken of, which is what an archived row IS. Restoring a row builds
 *  a new environment from that exact snapshot. */
export default async function Page({
  params,
}: {
  params: Promise<{ owner: string; id: string }>;
}) {
  const { owner, id } = await params;
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
        href={`/${owner}/environments`}
        className="flex items-center gap-1.5 text-sm2 text-muted-foreground transition-colors hover:text-foreground"
      >
        <ArrowLeft className="size-3.5" />Environments
      </Link>
      <h1 className="mt-2 flex items-center gap-2 text-body font-medium">
        <Camera className="size-4 text-muted-foreground" aria-hidden />
        {id}
      </h1>

      {history.value.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          No snapshots yet. Push the environment to take one.
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
                <span className="mt-1 block text-caption text-muted-foreground">
                  {when(new Date(c.created_at).getTime())}
                </span>
              </div>
              <RestoreEnvDialog owner={owner} snapshotId={c.id} />
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
