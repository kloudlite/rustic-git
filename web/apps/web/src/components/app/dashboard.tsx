import Link from "next/link";
import { ArrowUpRight, GitCommitHorizontal, Plus, Rocket, Tag, XCircle } from "lucide-react";
import { GlobalBar } from "@/components/app/global-bar";
import { Button } from "@/components/ui/button";
import { ACTIVITY, ENVIRONMENTS, REPOS, type Activity } from "@/lib/mock";
import type { Session } from "@/lib/session";

function PipelineDot({ state }: { state: "passing" | "failing" | "none" }) {
  const cls =
    state === "passing" ? "bg-[--color-success]" : state === "failing" ? "bg-destructive" : "bg-muted-foreground/40";
  return <span className={`size-[7px] shrink-0 ${cls}`} aria-hidden />;
}

function ActivityIcon({ kind, ok }: Pick<Activity, "kind" | "ok">) {
  const cls = ok === false ? "text-destructive" : "text-muted-foreground";
  const Icon =
    kind === "deploy" ? Rocket : kind === "release" ? Tag : kind === "pipeline" ? XCircle : GitCommitHorizontal;
  return <Icon className={`size-4 shrink-0 ${cls}`} />;
}

export function Dashboard({ session }: { session: NonNullable<Session> }) {
  const first = session.user.name.split(" ")[0];
  const failing = REPOS.filter((r) => r.pipeline === "failing").length;

  return (
    <div className="min-h-svh bg-background">
      <GlobalBar session={session} active="Repositories" />

      <main className="mx-auto max-w-[1120px] px-4 py-8 md:px-6">
        <div className="flex flex-wrap items-end justify-between gap-4">
          <div>
            <h1 className="text-[22px] font-bold tracking-tight">Good afternoon, {first}</h1>
            <p className="mt-1.5 text-[14px] text-muted-foreground">
              {REPOS.length} repositories
              {failing > 0 && (
                <>
                  {" · "}
                  <span className="font-medium text-destructive">{failing} pipeline failing</span>
                </>
              )}
            </p>
          </div>
          <Button className="font-semibold"><Plus className="size-4" />New repository</Button>
        </div>

        <div className="mt-8 grid gap-6 lg:grid-cols-[minmax(0,1fr)_320px]">
          {/* Repositories */}
          <section>
            <div className="mb-3 flex items-baseline justify-between">
              <h2 className="text-[13px] font-semibold uppercase tracking-[0.06em] text-muted-foreground">
                Repositories
              </h2>
              <Link href="/kloudlite" className="text-[13px] font-medium text-primary hover:underline">
                All repositories
              </Link>
            </div>

            <div className="border border-border">
              {REPOS.map((r, i) => (
                <Link
                  key={r.name}
                  href={`/kloudlite/${r.name}`}
                  className={`flex items-center gap-4 px-4 py-3.5 transition-colors hover:bg-muted/60 ${
                    i < REPOS.length - 1 ? "border-b border-border" : ""
                  }`}
                >
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2.5">
                      <span className="truncate text-[14.5px] font-semibold">{r.name}</span>
                      <span className="shrink-0 border border-border px-1.5 py-px text-[11px] font-medium text-muted-foreground">
                        {r.visibility}
                      </span>
                    </div>
                    <p className="mt-1 truncate text-[13px] text-muted-foreground">{r.description}</p>
                  </div>

                  <div className="hidden w-28 shrink-0 items-center gap-2 text-[12.5px] text-muted-foreground sm:flex">
                    <span className="size-2 shrink-0" style={{ background: r.language.color }} aria-hidden />
                    {r.language.name}
                  </div>

                  <div className="flex w-24 shrink-0 items-center justify-end gap-2 text-[12.5px] text-muted-foreground">
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
              <h2 className="mb-3 text-[13px] font-semibold uppercase tracking-[0.06em] text-muted-foreground">
                Environments
              </h2>
              <div className="border border-border">
                {ENVIRONMENTS.map((e, i) => (
                  <div
                    key={e.name}
                    className={`flex items-center gap-3 px-3.5 py-3 ${
                      i < ENVIRONMENTS.length - 1 ? "border-b border-border" : ""
                    }`}
                  >
                    <span
                      className={`size-[7px] shrink-0 ${e.healthy ? "bg-[--color-success]" : "bg-destructive"}`}
                      aria-hidden
                    />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-[13.5px] font-semibold">{e.name}</div>
                      <div className="truncate text-[12px] text-muted-foreground">{e.repo}</div>
                    </div>
                    <span className="shrink-0 font-mono text-[12px] text-primary">{e.sha}</span>
                  </div>
                ))}
              </div>
            </section>

            <section>
              <h2 className="mb-3 text-[13px] font-semibold uppercase tracking-[0.06em] text-muted-foreground">
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
                      <p className="text-[13px] leading-snug">{a.summary}</p>
                      <p className="mt-0.5 flex items-center gap-1.5 text-[12px] text-muted-foreground">
                        <span className="truncate">{a.repo}</span>
                        <span aria-hidden>·</span>
                        <span className="truncate font-mono">{a.detail}</span>
                      </p>
                    </div>
                    <span className="shrink-0 text-[12px] text-muted-foreground">{a.when}</span>
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
