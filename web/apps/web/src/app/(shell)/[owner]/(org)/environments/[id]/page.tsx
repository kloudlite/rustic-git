import { notFound } from "next/navigation";
import { Boxes } from "lucide-react";
import { loadEnvPage } from "@/lib/env-page";
import { listWorkspaces } from "@/lib/api";
import { interceptSummary } from "@/lib/intercept";
import { InterceptDialog, ReleaseIntercept } from "@/components/app/intercept-control";
import { requireToken } from "@/lib/session";

/** What the environment is RUNNING, right now.
 *
 *  Live environments only. An archived one runs nothing, so it is sent to its snapshots instead —
 *  the services a push recorded are the RESTORE's business (the api reads them off the record's
 *  provenance), and showing them here as though they were live is the one thing this page must
 *  never do. */
export default async function Page({ params }: { params: Promise<{ owner: string; id: string }> }) {
  const { owner, id } = await params;
  const { session, token } = await requireToken(`/${owner}/environments/${id}`);

  const page = await loadEnvPage(token, owner, id);
  if (!page) notFound();
  const { env, services } = page;
  // A deleted environment runs nothing. Say so here rather than redirecting: a redirect is a
  // second navigation that the tab row has to catch up with, which is what made opening one
  // look like a jump. The list's Snapshots section already links these rows here.
  if (!env) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <Boxes className="mx-auto size-6 text-muted-foreground" aria-hidden />
        <p className="mt-3 text-sm2 font-medium">Deleted — nothing is running</p>
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

  // Candidates for an intercept: the viewer's workspaces in this environment's region. Whether
  // one is ATTACHED is the api's to know — the workspace document does not carry it — and it
  // refuses an unattached one with a sentence the dialog shows. A failed read leaves the button
  // disabled rather than failing the page: the services above are what someone came here for.
  const scope = owner === session.user.owner ? undefined : owner;
  const wsRes = await listWorkspaces(token, scope);
  const candidates = (wsRes.ok ? wsRes.value : [])
    .filter((w) => w.region === env.region && w.state === "ready")
    .map((w) => ({ id: w.id, name: w.name }));

  return (
    <>
      <ul className="mt-5 divide-y divide-border border border-border bg-card">
        {services.map((s) => {
          // Two different questions, and the row must never answer one with the other:
          // `intercepted_by` is what traffic is ACTUALLY doing, `wish` is what was asked for.
          // A wish with no `intercepted_by` is its own state — the workspace is stopped or
          // unreachable, and the real service is up and answering.
          const inForce = s.intercepted_by ?? null;
          const wish = interceptSummary(s, env.intercepts);
          // DESCRIPTIVE, not authoritative: this is the wish's mapping. The api exposes no
          // in-force port list — `intercepted_by` is the whole of what status says — so this is
          // the closest honest answer to "where does it land", and it is the same mapping the
          // controller applied unless the wish has been rewritten since.
          const mapping = wish.ports.map((m) => `${m.service} → ${m.workspace}`).join(", ");
          return (
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
            {/* Release is offered for a WISH, in force or not: an intercept waiting on a stopped
                workspace is exactly the thing someone comes here to undo, and releasing is the
                only thing that drops it. */}
            {inForce || wish.heldBy ? (
              <ReleaseIntercept owner={owner} id={id} service={s.name} />
            ) : (
              <InterceptDialog owner={owner} id={id} service={s.name} ports={s.ports} workspaces={candidates} />
            )}
            {s.command.length > 0 && (
              <div className="w-full truncate font-mono text-caption text-muted-foreground">
                {s.command.join(" ")}
              </div>
            )}
            {inForce ? (
              <p className="w-full text-caption text-warning">
                Intercepted by <span className="font-mono">{inForce}</span>, service stopped
                {mapping && <> · ports {mapping}</>}
              </p>
            ) : wish.heldBy ? (
              <p className="w-full text-caption text-muted-foreground">
                Intercept set for <span className="font-mono">{wish.heldBy}</span>, not in force — that
                workspace is stopped or unreachable, so the real service is answering. It takes hold
                again by itself when the workspace comes back.
              </p>
            ) : null}
          </li>
          );
        })}
      </ul>
      <p className="mt-3 text-caption text-muted-foreground">
        Reach a service from another in the same environment as <span className="font-mono">name:port</span> —
        CoreDNS resolves inside its namespace.
      </p>
    </>
  );
}
