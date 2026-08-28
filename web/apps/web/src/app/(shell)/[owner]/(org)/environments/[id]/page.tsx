import { notFound, redirect } from "next/navigation";
import { Boxes } from "lucide-react";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { loadEnvPage } from "@/lib/env-page";

/** What the environment RUNS. For an archived one there is nothing running, so the list comes
 *  from the newest snapshot's provenance — a push records the services precisely so a restore of
 *  a deleted environment can come back up as something, and a record written before that carries
 *  none, which the page says rather than showing an empty table as if it were the truth. */
export default async function Page({ params }: { params: Promise<{ owner: string; id: string }> }) {
  const { owner, id } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  const token = await apiToken();
  if (!token) redirect("/login");

  const page = await loadEnvPage(token, owner, id);
  if (!page) notFound();
  const { env, services } = page;

  if (services.length === 0) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <Boxes className="mx-auto size-6 text-muted-foreground" aria-hidden />
        <p className="mt-3 text-sm2 font-medium">{env ? "No services" : "No services recorded"}</p>
        <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
          {env
            ? "This environment holds data and runs nothing."
            : "This snapshot was taken before pushes recorded them. Restoring brings the data back, with no services."}
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
      {env && (
        <p className="mt-3 text-caption text-muted-foreground">
          Reach a service from another in the same environment as <span className="font-mono">name:port</span> —
          CoreDNS resolves inside its namespace.
        </p>
      )}
    </>
  );
}
