import Link from "next/link";
import { CircleCheck, CircleX, GitCommitHorizontal, Layers, Minus, Plus, Rocket, Search, SquareCode, SquareTerminal, Tag, XCircle, Zap } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { ACTIVITY, REPOS, type Activity } from "@/lib/mock";
import type { Session } from "@/lib/session";
import { Badge } from "@/components/ui/badge";

/** Last pipeline result. An icon, not a coloured square: a square beside a name
 *  reads as a second badge, and colour alone is not a signal everyone can read. */
function PipelineStatus({ state }: { state: "passing" | "failing" | "none" }) {
  if (state === "passing") return <CircleCheck className="size-4 shrink-0 text-success" aria-label="Pipeline passing" />;
  if (state === "failing") return <CircleX className="size-4 shrink-0 text-destructive" aria-label="Pipeline failing" />;
  return <Minus className="size-4 shrink-0 text-muted-foreground/50" aria-label="No pipeline" />;
}

function ActivityIcon({ kind, ok }: Pick<Activity, "kind" | "ok">) {
  const cls = ok === false ? "text-destructive" : "text-muted-foreground";
  const Icon =
    kind === "deploy" ? Rocket : kind === "release" ? Tag : kind === "pipeline" ? XCircle : GitCommitHorizontal;
  return <Icon className={`size-4 shrink-0 ${cls}`} />;
}

/** The three team repos are drawn with the icon of the section they feed, so the
 *  list says what they are without a label. Everything else is a plain repo. */
function RepoIcon({ system }: { system?: "workspaces" | "environments" | "actions" }) {
  const cls = "size-4 shrink-0";
  if (system === "workspaces") return <SquareTerminal className={`${cls} text-primary`} aria-label="Team workspaces repo" />;
  if (system === "environments") return <Layers className={`${cls} text-primary`} aria-label="Team environments repo" />;
  if (system === "actions") return <Zap className={`${cls} text-primary`} aria-label="Team CI repo" />;
  return <SquareCode className={`${cls} text-muted-foreground`} />;
}

/** Home for a signed-in user is the Code Repos list. The section tab already names
 *  the page, so there is no title to repeat: one toolbar row — filter, scope, count,
 *  primary action — then the list. Every list page in the product shares this shape. */
export function Dashboard({ session }: { session: NonNullable<Session> }) {
  return (
    <AppShell session={session} active="Code Repos">
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <div className="grid gap-10 xl:grid-cols-overview">
          <section>
            <div className="flex flex-wrap items-center gap-3">
              <div className="relative w-full max-w-xs">
                <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
                <Input placeholder="Filter repos" className="h-8 pl-8 text-sm2" aria-label="Filter repos" />
              </div>
              <Tabs defaultValue="all">
                <TabsList className="h-8">
                  <TabsTrigger value="all" className="text-sm2">All</TabsTrigger>
                  <TabsTrigger value="public" className="text-sm2">Public</TabsTrigger>
                  <TabsTrigger value="private" className="text-sm2">Private</TabsTrigger>
                </TabsList>
              </Tabs>
              <Button className="ml-auto"><Plus />New repo</Button>
            </div>

            <div className="mt-5 border border-border">
              {REPOS.map((r, i) => (
                <Link
                  key={r.name}
                  href={`/${session.user.owner}/${r.name}`}
                  className={`flex items-center gap-6 px-5 py-4 transition-colors hover:bg-muted/60 ${
                    i < REPOS.length - 1 ? "border-b border-border" : ""
                  }`}
                >
                  <RepoIcon system={r.system} />
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2.5">
                      <span className="truncate text-body font-medium">{r.name}</span>
                      <Badge variant="outline">
                        {r.visibility}
                      </Badge>
                    </div>
                    <p className="mt-1 truncate text-sm2 text-muted-foreground">{r.description}</p>
                  </div>

                  <div className="flex shrink-0 items-center gap-4 text-caption text-muted-foreground">
                    <span className="hidden md:inline">{r.updated}</span>
                    <PipelineStatus state={r.pipeline} />
                  </div>
                </Link>
              ))}
            </div>
          </section>

          <aside>
            <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
              Activity
            </h2>
            <div className="mt-4 border border-border">
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
