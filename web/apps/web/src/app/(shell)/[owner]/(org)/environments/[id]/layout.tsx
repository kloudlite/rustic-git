import { notFound, redirect } from "next/navigation";
import { SetCrumbTitle } from "@/components/app/shell-context";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import { EnvHeaderActions } from "@/components/app/env-actions";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { loadEnvPage } from "@/lib/env-page";

/** An environment is a SUBJECT, like a repo or an image: entering one swaps the chrome's tab row
 *  for its own (Services | Snapshots, with the arrow back to the list) and grows the breadcrumb a
 *  segment. That row lives in the shell, which stays mounted across navigations — a tab row torn
 *  down and rebuilt per page cannot slide, it can only reappear. So this layout owns the header
 *  and nothing else, and tells the shell the one thing the URL cannot say: the environment's name,
 *  since the URL carries its id.
 *
 *  `loadEnvPage` is `cache()`d, so the pages below share this one read. */
export default async function Layout({
  children,
  params,
}: {
  children: React.ReactNode;
  params: Promise<{ owner: string; id: string }>;
}) {
  const { owner, id } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");
  const token = await apiToken();
  if (!token) redirect("/login");

  const page = await loadEnvPage(token, owner, id);
  if (!page) notFound();
  const { env, history } = page;

  return (
    <section className="min-w-0">
      <SetCrumbTitle title={page.name} archived={!env} />
      <header>
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="truncate text-title font-semibold">{page.name}</h1>
          {env ? (
            <>
              <WsEnvStateBadge state={env.state} />
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
          ) : (
            <span className="border border-border px-1.5 py-0.5 text-caption text-muted-foreground">archived</span>
          )}
          <span className="ml-auto flex items-center gap-2">
            <EnvHeaderActions owner={owner} id={id} name={page.name} state={env?.state ?? null} />
          </span>
        </div>
        <p className="mt-1 text-sm2 text-muted-foreground">
          {env ? (
            <>
              {env.region}
              {env.placement ? ` · ${env.placement}` : " · not placed yet"} · {page.services.length}{" "}
              {page.services.length === 1 ? "service" : "services"}
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
