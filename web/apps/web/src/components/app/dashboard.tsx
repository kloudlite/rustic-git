import { GitCommitHorizontal, Rocket, Tag, XCircle } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { RepoList } from "@/components/app/repo-list";
import { ACTIVITY, type Activity } from "@/lib/mock";
import type { ApiRepo } from "@/lib/api";
import type { Session } from "@/lib/session";

function ActivityIcon({ kind, ok }: Pick<Activity, "kind" | "ok">) {
  const cls = ok === false ? "text-destructive" : "text-muted-foreground";
  const Icon =
    kind === "deploy" ? Rocket : kind === "release" ? Tag : kind === "pipeline" ? XCircle : GitCommitHorizontal;
  return <Icon className={`size-4 shrink-0 ${cls}`} />;
}

/** Home for a signed-in user is the Code Repos list. The section tab already names
 *  the page, so there is no title to repeat: one toolbar row — filter, scope, count,
 *  primary action — then the list. Every list page in the product shares this shape. */
export function Dashboard({
  session,
  owner,
  repos,
}: {
  session: NonNullable<Session>;
  owner: string;
  repos: ApiRepo[];
}) {
  return (
    <AppShell session={session}>
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <div className="grid gap-10 xl:grid-cols-overview">
          <section className="min-w-0">
            <RepoList owner={owner} repos={repos} />
          </section>

          <aside>
            <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
              Activity
            </h2>
            <div className="mt-4 border border-border bg-card">
              {ACTIVITY.map((a, i) => (
                <div
                  key={`${a.repo}-${i}`}
                  className={`flex items-start gap-3 px-4 py-3.5 ${
                    i < ACTIVITY.length - 1 ? "border-b border-border" : ""
                  }`}
                >
                  <span className="mt-0.5"><ActivityIcon kind={a.kind} ok={a.ok} /></span>
                  <div className="min-w-0 flex-1">
                    <p className="text-sm2 leading-snug">{a.summary}</p>
                    <p className="mt-1 flex items-center gap-1.5 text-caption text-muted-foreground">
                      <span className="truncate">{a.repo}</span>
                      <span aria-hidden>·</span>
                      <span className="truncate font-mono">{a.detail}</span>
                    </p>
                  </div>
                  <span className="shrink-0 text-caption text-muted-foreground">{a.when}</span>
                </div>
              ))}
            </div>
          </aside>
        </div>
      </main>
    </AppShell>
  );
}
