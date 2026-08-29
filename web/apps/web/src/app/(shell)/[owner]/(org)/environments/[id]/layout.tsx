import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { SetCrumbTitle } from "@/components/app/shell-context";
import { EnvHeaderActions } from "@/components/app/env-actions";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { loadEnvPage } from "@/lib/env-page";
import { apiToken } from "@/lib/api-token";
import { when } from "@/lib/time";
import { requireToken } from "@/lib/session";

/** An environment is a SUBJECT, like a repo or an image: entering one swaps the chrome's tab row
 *  for its own (Services | Snapshots, with the arrow back to the list) and grows the breadcrumb a
 *  segment. That row lives in the shell, which stays mounted across navigations — a tab row torn
 *  down and rebuilt per page cannot slide, it can only reappear. So this layout owns the header
 *  and nothing else, and tells the shell the one thing the URL cannot say: the environment's name,
 *  since the URL carries its id.
 *
 *  `loadEnvPage` is `cache()`d, so the pages below share this one read. */
/** Named after the environment, not its id: `loadEnvPage` is `cache()`d, so this is the
 *  same read the layout body makes. Signed out, the layout's guard redirects anyway. */
export async function generateMetadata({ params }: { params: Promise<{ owner: string; id: string }> }): Promise<Metadata> {
  const { owner, id } = await params;
  const token = await apiToken();
  const page = token ? await loadEnvPage(token, owner, id) : null;
  return { title: page?.name ?? id };
}

export default async function Layout({
  children,
  params,
}: {
  children: React.ReactNode;
  params: Promise<{ owner: string; id: string }>;
}) {
  const { owner, id } = await params;
  const { token } = await requireToken(`/${owner}/environments/${id}`);

  const page = await loadEnvPage(token, owner, id);
  if (!page) notFound();
  const { env, history } = page;
  const at = env ? (history.find((c) => c.id === env.restored_to) ?? history[0] ?? null) : null;

  return (
    <section className="min-w-0">
      <AutoRefresh />
      {/* The crumb carries the name and the state, as a repo's does; the header below is only
          the actions and the facts, so the page reads like Code Repos rather than a document. */}
      <SetCrumbTitle title={page.name} archived={!env} badge={env ? env.state : "archived"} />
      <header>
        <div className="flex flex-wrap items-center gap-3">
          {env ? (
            <>
              {/* A restore takes the services down and swaps the disk; without this the page shows
                  an ordinary state and the operator reads it as a restart that will not finish. */}
              {env.restoring && (
                <span
                  title={`Restoring (${env.restoring})`}
                  className="border border-warning/40 bg-warning/10 px-1.5 py-0.5 text-caption text-warning"
                >
                  restoring…
                </span>
              )}
            </>
          ) : null}
          <span className="ml-auto flex items-center gap-2">
            <EnvHeaderActions owner={owner} id={id} state={env?.state ?? null} />
          </span>
        </div>
        <p className="mt-1 text-sm2 text-muted-foreground">
          {env ? (
            <>
              {env.region}
              {env.placement ? ` · ${env.placement}` : " · not placed yet"} · {page.services.length}{" "}
              {page.services.length === 1 ? "service" : "services"}
              {/* Where the lineage is, without opening the Snapshots tab. Same rule the tab uses:
                  `restored_to` when set, else the newest record. */}
              {at && (
                <>
                  {" · at "}
                  <span className={at.message ? "" : "text-muted-foreground"}>&ldquo;{at.message || "snapshot"}&rdquo;</span>
                  {" · "}
                  {when(new Date(at.created_at).getTime())}
                </>
              )}
            </>
          ) : (
            // Nothing but snapshots is left, so the meta line says what those snapshots are of
            // rather than repeating a region the environment no longer runs in.
            <>
              {history.length} {history.length === 1 ? "snapshot" : "snapshots"} · the environment is gone; its
              data is not
            </>
          )}
        </p>
      </header>
      {children}
    </section>
  );
}
