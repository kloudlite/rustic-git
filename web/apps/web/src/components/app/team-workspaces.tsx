import Link from "next/link";
import { ArrowRight, Bot, ExternalLink, FileCode, GitBranch, Plus, Search, Square } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { Initials } from "@/components/app/initials";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { WORKSPACE_DEFINITIONS, WORKSPACE_SESSIONS, type WorkspaceSession } from "@/lib/mock";
import type { Session } from "@/lib/session";

/** Who a session belongs to. A person is initials; an agent is the bot mark with
 *  its name and whose behalf it works on — the two kinds are the whole point. */
function Owner({ owner }: { owner: WorkspaceSession["owner"] }) {
  if (owner.kind === "user") {
    return (
      <span className="flex items-center gap-2">
        <Initials name={owner.name} size={6} />
        <span className="text-sm2 font-medium">{owner.login}</span>
      </span>
    );
  }
  return (
    <span className="flex items-center gap-2">
      <span className="flex size-6 items-center justify-center bg-primary/10 text-primary"><Bot className="size-3.5" /></span>
      <span className="text-sm2">
        <span className="font-medium">{owner.name}</span>
        <span className="text-muted-foreground"> · agent for {owner.for}</span>
      </span>
    </span>
  );
}

function StatusDot({ status }: { status: WorkspaceSession["status"] }) {
  const cls = status === "running" ? "bg-success" : status === "idle" ? "bg-warning" : "bg-muted-foreground/40";
  return <span className={`size-1.5 shrink-0 ${cls}`} aria-label={status} />;
}

/** Workspaces are sessions: a definition brought up on a repo at a ref, for a person
 *  or an agent. The list is the sessions; the rail is the definitions they start
 *  from, which live as code in the `.workspaces` repo. */
export function TeamWorkspaces({ session }: { session: NonNullable<Session> }) {
  const owner = session.user.owner;
  const running = WORKSPACE_SESSIONS.filter((s) => s.status === "running").length;
  const agents = WORKSPACE_SESSIONS.filter((s) => s.owner.kind === "agent" && s.status !== "stopped").length;

  return (
    <AppShell session={session} active="Workspaces">
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <div className="grid gap-10 xl:grid-cols-overview">
          <section>
            <div className="flex flex-wrap items-center gap-3">
              <div className="relative w-full max-w-xs">
                <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
                <Input placeholder="Filter workspaces" className="h-8 pl-8" aria-label="Filter workspaces" />
              </div>
              <Tabs defaultValue="all">
                <TabsList>
                  <TabsTrigger value="all">All {WORKSPACE_SESSIONS.length}</TabsTrigger>
                  <TabsTrigger value="running">Running {running}</TabsTrigger>
                  <TabsTrigger value="mine">Mine</TabsTrigger>
                  <TabsTrigger value="agents">Agents {agents}</TabsTrigger>
                </TabsList>
              </Tabs>
              <Button className="ml-auto"><Plus />New workspace</Button>
            </div>

            <ul className="mt-5 divide-y divide-border border border-border">
              {WORKSPACE_SESSIONS.map((w) => (
                <li key={w.id} className="grid grid-cols-sessions items-center gap-4 px-5 py-3.5">
                  <StatusDot status={w.status} />
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                      <span className="text-sm2 font-medium">{w.definition}</span>
                      <span className="text-caption text-muted-foreground">on</span>
                      <Link href={`/${owner}/${w.repo}`} className="text-sm2 font-medium underline-offset-4 hover:underline">{w.repo}</Link>
                      <span className="inline-flex items-center gap-1 font-mono text-caption text-muted-foreground"><GitBranch className="size-3" />{w.ref}</span>
                      {w.agents > 0 && (
                        <Tooltip>
                          <TooltipTrigger asChild>
                            <Badge variant="outline" className="gap-1"><Bot className="size-3" />{w.agents}</Badge>
                          </TooltipTrigger>
                          <TooltipContent>{w.agents} agent{w.agents > 1 ? "s" : ""} attached to this session</TooltipContent>
                        </Tooltip>
                      )}
                    </div>
                    <div className="mt-1 flex flex-wrap items-center gap-x-3 text-caption text-muted-foreground">
                      <span>started {w.started}</span>
                      <span aria-hidden>·</span>
                      <span>active {w.active}</span>
                      {w.cpu && <><span aria-hidden>·</span><span className="font-mono">{w.cpu}</span></>}
                    </div>
                  </div>
                  <Owner owner={w.owner} />
                  <div className="flex items-center justify-end gap-2">
                    {w.status !== "stopped" ? (
                      <>
                        <Button asChild variant="outline" size="sm" className="border-edge hover:border-edge-hover">
                          <a href="#" aria-label={`Open ${w.definition} on ${w.repo}`}>Open <ExternalLink /></a>
                        </Button>
                        <Button variant="ghost" size="icon-sm" aria-label="Stop" className="text-muted-foreground hover:text-destructive"><Square /></Button>
                      </>
                    ) : (
                      <Button variant="outline" size="sm" className="border-edge hover:border-edge-hover">Start</Button>
                    )}
                  </div>
                </li>
              ))}
            </ul>
          </section>

          <aside className="grid content-start gap-6">
            <section>
              <div className="flex items-baseline justify-between">
                <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">Definitions</h2>
                <Link href={`/${owner}/.workspaces`} className="inline-flex items-center gap-1 font-mono text-caption text-muted-foreground underline-offset-4 hover:text-foreground hover:underline">
                  .workspaces <ArrowRight className="size-3" />
                </Link>
              </div>
              <p className="mt-2 text-caption text-muted-foreground">
                What a workspace starts from. Managed as code — change the file, and the next session picks it up.
              </p>
              <ul className="mt-3 divide-y divide-border border border-border">
                {WORKSPACE_DEFINITIONS.map((d) => (
                  <li key={d.name} className="px-4 py-3">
                    <div className="flex items-center gap-2">
                      <span className="text-sm2 font-medium">{d.name}</span>
                      <Badge variant="outline" className="font-mono">{d.image}</Badge>
                      <span className="ml-auto text-caption text-muted-foreground">{d.sessions} live</span>
                    </div>
                    <div className="mt-1 flex flex-wrap gap-1">
                      {d.tools.map((t) => <span key={t} className="font-mono text-micro text-muted-foreground">{t}</span>).reduce<React.ReactNode[]>((acc, el, i) => (i ? [...acc, <span key={`s${i}`} className="text-micro text-muted-foreground/60">·</span>, el] : [el]), [])}
                    </div>
                    <Link href={`/${owner}/.workspaces/blob/${d.path}`} className="mt-1.5 inline-flex items-center gap-1.5 font-mono text-caption text-muted-foreground underline-offset-4 transition-colors hover:text-foreground hover:underline">
                      <FileCode className="size-3.5" />{d.path}
                    </Link>
                  </li>
                ))}
              </ul>
            </section>
          </aside>
        </div>
      </main>
    </AppShell>
  );
}
