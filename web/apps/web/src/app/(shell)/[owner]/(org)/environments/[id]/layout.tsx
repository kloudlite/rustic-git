import { notFound, redirect } from "next/navigation";
import { Boxes, Camera } from "lucide-react";
import { BackLink } from "@/components/repo/back-link";
import { NavTabs } from "@/components/app/nav-tabs";
import { WsEnvStateBadge } from "@/components/app/wsenv-state-badge";
import { EnvHeaderActions } from "@/components/app/env-actions";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { loadEnvPage } from "@/lib/env-page";

/** The header and the tab row live here rather than in each tab, so switching tabs does not
 *  re-mount them — that is what makes the underline slide instead of blink. `loadEnvPage` is
 *  `cache()`d, so the pages below share this one read. */
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
  const base = `/${owner}/environments/${encodeURIComponent(id)}`;

  return (
    <section className="min-w-0">
      <BackLink href={`/${owner}/environments`}>Environments</BackLink>
      <header className="mt-3">
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="truncate text-title font-semibold">{page.name}</h1>
          {env ? (
            <WsEnvStateBadge state={env.state} />
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

        <div className="mt-5 border-b border-border">
          <NavTabs
            aria-label="Environment"
            tabs={[
              { href: base, label: "Services", icon: <Boxes />, count: page.services.length, exact: true },
              { href: `${base}/snapshots`, label: "Snapshots", icon: <Camera />, count: history.length },
            ]}
          />
        </div>
      </header>
      {children}
    </section>
  );
}
