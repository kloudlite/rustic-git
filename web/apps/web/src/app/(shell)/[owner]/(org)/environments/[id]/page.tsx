import { notFound } from "next/navigation";
import { Boxes } from "lucide-react";
import { loadEnvPage } from "@/lib/env-page";
import { requireToken } from "@/lib/session";

/** What the environment is RUNNING, right now.
 *
 *  Live environments only. An archived one runs nothing, so it is sent to its snapshots instead —
 *  the services a push recorded are the RESTORE's business (the api reads them off the record's
 *  provenance), and showing them here as though they were live is the one thing this page must
 *  never do. */
export default async function Page({ params }: { params: Promise<{ owner: string; id: string }> }) {
  const { owner, id } = await params;
  const { token } = await requireToken(`/${owner}/environments/${id}`);

  const page = await loadEnvPage(token, owner, id);
  if (!page) notFound();
  const { env, services } = page;
  // An archived environment runs nothing. Say so here rather than redirecting: a redirect is a
  // second navigation that the tab row has to catch up with, which is what made opening one
  // archived environment look like a jump. The list already links archived rows to Snapshots.
  if (!env) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <Boxes className="mx-auto size-6 text-muted-foreground" aria-hidden />
        <p className="mt-3 text-sm2 font-medium">Archived — nothing is running</p>
        <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
          The environment was deleted and its snapshots kept. Restore one to run it again.
        </p>
      </div>
    );
  }

  if (services.length === 0) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <Boxes className="mx-auto size-6 text-muted-foreground" aria-hidden />
        <p className="mt-3 text-sm2 font-medium">No services</p>
        <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
          This environment holds data and runs nothing.
        </p>
      </div>
    );
  }

  return (
    <>
      <ul className="mt-5 divide-y divide-border border border-border bg-card">
        {services.map((s) => (
          <li key={s.name} className="flex flex-wrap items-center gap-4 px-5 py-3.5">
            <div className="min-w-0 flex-1">
              <div className="truncate text-body font-medium">{s.name}</div>
              <div className="mt-0.5 truncate font-mono text-sm2 text-muted-foreground">{s.image}</div>
            </div>
            <div className="min-w-0 text-sm2 text-muted-foreground">
              {/* Mounts, not ports and not readiness: the api's service doc carries neither, and
                  a column that can only ever be blank is a column that lies about what is known. */}
              {s.mounts.length === 0
                ? "no volumes"
                : s.mounts.map((m) => `${m.folder} → ${m.path}`).join(", ")}
            </div>
            {s.command.length > 0 && (
              <div className="w-full truncate font-mono text-caption text-muted-foreground">
                {s.command.join(" ")}
              </div>
            )}
          </li>
        ))}
      </ul>
      <p className="mt-3 text-caption text-muted-foreground">
        Reach a service from another in the same environment as <span className="font-mono">name:port</span> —
        CoreDNS resolves inside its namespace.
      </p>
    </>
  );
}
