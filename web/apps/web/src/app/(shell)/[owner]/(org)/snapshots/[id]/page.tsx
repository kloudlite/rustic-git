import Link from "next/link";
import { ArrowLeft, Camera } from "lucide-react";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { volumeHistory } from "@/lib/api";
import { when } from "@/lib/time";

/** One row per commit, newest first — the api's own contract for `history`. */
export default async function Page({ params }: { params: Promise<{ owner: string; id: string }> }) {
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
          No commits yet.
        </p>
      ) : (
        <ul className="mt-5 divide-y divide-border border border-border bg-card">
          {history.value.map((c) => {
            const blocks = c.lineage.filter((l) => l.kind === "block").length;
            const streams = c.lineage.filter((l) => l.kind === "stream").length;
            const layers = [
              blocks > 0 && `${blocks} block`,
              streams > 0 && `${streams} stream`,
            ].filter(Boolean).join(" + ") || "no layers";
            const sha = c.lineage.at(-1)?.sha256;
            return (
              <li key={c.id} className="flex flex-wrap items-center gap-4 px-5 py-4">
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-2.5">
                    <span className="font-mono text-sm2">{c.id.slice(0, 8)}</span>
                    <span className={`truncate text-sm2 ${c.message ? "text-foreground" : "text-muted-foreground/50 italic"}`}>
                      {c.message || "—"}
                    </span>
                  </span>
                  <span className="mt-1 block text-caption text-muted-foreground">
                    {when(new Date(c.created_at).getTime())} · {layers}
                    {sha && <> · <span className="font-mono">{sha.slice(0, 8)}</span></>}
                  </span>
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
