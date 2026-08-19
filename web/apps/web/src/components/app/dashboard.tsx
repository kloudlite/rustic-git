import Link from "next/link";
import { ArrowUpRight, GitCommitHorizontal, Plus, Rocket, Tag, XCircle } from "lucide-react";
import { GlobalBar } from "@/components/app/global-bar";
import { Button } from "@/components/ui/button";
import { ACTIVITY, REPOS, type Activity } from "@/lib/mock";
import type { Session } from "@/lib/session";

function PipelineDot({ state }: { state: "passing" | "failing" | "none" }) {
  const cls =
    state === "passing" ? "bg-success" : state === "failing" ? "bg-destructive" : "bg-muted-foreground/40";
  return <span className={`size-1.75 shrink-0 ${cls}`} aria-hidden />;
}

function ActivityIcon({ kind, ok }: Pick<Activity, "kind" | "ok">) {
  const cls = ok === false ? "text-destructive" : "text-muted-foreground";
  const Icon =
    kind === "deploy" ? Rocket : kind === "release" ? Tag : kind === "pipeline" ? XCircle : GitCommitHorizontal;
  return <Icon className={`size-4 shrink-0 ${cls}`} />;
}

export function Dashboard({ session }: { session: NonNullable<Session> }) {
  const failing = REPOS.filter((r) => r.pipeline === "failing").length;

  return (
    <div className="min-h-svh bg-background">
      {/* No section is active: this is home, not a page inside Code Repos. Marking
          one made the nav claim a location the page then contradicted. */}
      <GlobalBar session={session} />

      <main className="mx-auto max-w-page px-4 py-8 md:px-6">
        <div className="flex flex-wrap items-end justify-between gap-4">
          <div>
            <h1 className="text-title font-semibold tracking-title">Overview</h1>
            <p className="mt-1 text-sm2 text-muted-foreground">
              {REPOS.length} repos
              {failing > 0 && (
                <>
                  {" · "}
                  <span className="font-medium text-destructive">{failing} pipeline failing</span>
                </>
              )}
            </p>
          </div>
          <Button><Plus />New repo</Button>
        </div>

        <div className="mt-8 grid gap-6 lg:grid-cols-overview">
          {/* Repositories */}
          <section>
            <div className="mb-3 flex items-baseline justify-between">
              <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
                Code Repos
              </h2>
              <Link href="/kloudlite" className="text-sm2 font-medium text-primary underline-offset-4 hover:underline">
                All repos
              </Link>
            </div>

            <div className="border border-border">
              {REPOS.map((r, i) => (
                <Link
                  key={r.name}
                  href={`/kloudlite/${r.name}`}
                  className={`flex items-center gap-4 px-4 py-3 transition-colors hover:bg-muted/60 ${
                    i < REPOS.length - 1 ? "border-b border-border" : ""
                  }`}
                >
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2.5">
                      <span className="truncate text-body font-medium">{r.name}</span>
                      <span className="shrink-0 border border-border px-1.5 py-px text-micro font-medium text-muted-foreground">
                        {r.visibility}
                      </span>
                    </div>
                    <p className="mt-1 truncate text-sm2 text-muted-foreground">{r.description}</p>
                  </div>

                  <div className="hidden w-28 shrink-0 items-center gap-2 text-caption text-muted-foreground sm:flex">
                    <span className="size-2 shrink-0" style={{ background: r.language.color }} aria-hidden />
                    {r.language.name}
                  </div>

                  <div className="flex w-24 shrink-0 items-center justify-end gap-2 text-caption text-muted-foreground">
                    <PipelineDot state={r.pipeline} />
                    <span className="hidden md:inline">{r.updated}</span>
                  </div>
                </Link>
              ))}
            </div>
          </section>

          {/* Right rail */}
          <div className="grid gap-6">
            <section>
              <h2 className="mb-3 text-caption font-semibold uppercase tracking-label text-muted-foreground">
                Activity
              </h2>
              <div className="border border-border">
                {ACTIVITY.map((a, i) => (
                  <div
                    key={`${a.repo}-${i}`}
                    className={`flex items-start gap-3 px-3.5 py-3 ${
                      i < ACTIVITY.length - 1 ? "border-b border-border" : ""
                    }`}
                  >
                    <span className="mt-0.5"><ActivityIcon kind={a.kind} ok={a.ok} /></span>
                    <div className="min-w-0 flex-1">
                      <p className="text-sm2 leading-snug">{a.summary}</p>
                      <p className="mt-0.5 flex items-center gap-1.5 text-caption text-muted-foreground">
                        <span className="truncate">{a.repo}</span>
                        <span aria-hidden>·</span>
                        <span className="truncate font-mono">{a.detail}</span>
                      </p>
                    </div>
                    <span className="shrink-0 text-caption text-muted-foreground">{a.when}</span>
                  </div>
                ))}
              </div>
            </section>
          </div>
        </div>
      </main>
    </div>
  );
}
