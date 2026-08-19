import Link from "next/link";
import { ArrowRight, Bot, ExternalLink, FileCode, MoreHorizontal, Plus, Search } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { Initials } from "@/components/app/initials";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { WORKSPACE_DEFINITIONS, WORKSPACE_SESSIONS, type WorkspaceSession } from "@/lib/mock";
import type { Session } from "@/lib/session";

function Owner({ owner }: { owner: WorkspaceSession["owner"] }) {
  if (owner.kind === "user") {
    return (
      <span className="flex items-center gap-2 text-sm2">
        <Initials name={owner.name} size={6} />
        {owner.login}
      </span>
    );
  }
  return (
    <span className="flex items-center gap-2 text-sm2">
      <span className="flex size-6 shrink-0 items-center justify-center bg-primary/10 text-primary"><Bot className="size-3.5" /></span>
      <span className="min-w-0">
        <span className="block truncate">{owner.name}</span>
        <span className="block truncate text-caption text-muted-foreground">agent · for {owner.for}</span>
      </span>
    </span>
  );
}

function Status({ status }: { status: WorkspaceSession["status"] }) {
  const cls = status === "running" ? "bg-success" : status === "idle" ? "bg-warning" : "bg-muted-foreground/40";
  return (
    <span className="flex items-center gap-2 text-caption text-muted-foreground">
      <span className={`size-1.5 shrink-0 ${cls}`} aria-hidden />
      <span className="capitalize">{status}</span>
    </span>
  );
}

/** Workspaces are sessions: a definition brought up on a repo at a ref, for a person
 *  or an agent. One table, four labelled columns; the definitions they start from
 *  sit in the rail and live as code in the `.workspaces` repo. */
export function TeamWorkspaces({ session }: { session: NonNullable<Session> }) {
  const owner = session.user.owner;
  const running = WORKSPACE_SESSIONS.filter((s) => s.status === "running").length;
  const agents = WORKSPACE_SESSIONS.filter((s) => s.owner.kind === "agent" && s.status !== "stopped").length;

  return (
    <AppShell session={session} active="Workspaces">
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <div className="grid gap-10 xl:grid-cols-overview">
          <section>
            <div className="flex items-center gap-3">
              <div className="relative w-56">
                <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
                <Input placeholder="Filter" className="h-8 pl-8" aria-label="Filter workspaces" />
              </div>
              <Tabs defaultValue="all">
                <TabsList>
                  <TabsTrigger value="all">All <span className="text-muted-foreground">{WORKSPACE_SESSIONS.length}</span></TabsTrigger>
                  <TabsTrigger value="running">Running <span className="text-muted-foreground">{running}</span></TabsTrigger>
                  <TabsTrigger value="agents">Agents <span className="text-muted-foreground">{agents}</span></TabsTrigger>
                </TabsList>
              </Tabs>
              <Button className="ml-auto"><Plus />New workspace</Button>
            </div>

            <div className="mt-5 border border-border">
              <div className="grid grid-cols-sessions items-center gap-4 border-b border-border bg-muted/40 px-4 py-2 text-micro font-semibold uppercase tracking-label text-muted-foreground">
                <span>Workspace</span>
                <span>Owner</span>
                <span>Status</span>
                <span className="text-right">Active</span>
                <span />
              </div>
              <ul className="divide-y divide-border">
                {WORKSPACE_SESSIONS.map((w) => (
                  <li key={w.id} className="grid grid-cols-sessions items-center gap-4 px-4 py-3">
                    <div className="min-w-0">
                      <div className="flex items-center gap-2">
                        <span className="truncate text-sm2 font-medium">{w.definition}</span>
                        {w.agents > 0 && (
                          <Badge variant="outline" className="gap-1" title={`${w.agents} agents attached`}>
                            <Bot className="size-3" />{w.agents}
                          </Badge>
                        )}
                      </div>
                      <div className="mt-0.5 truncate font-mono text-caption text-muted-foreground">
                        <Link href={`/${owner}/${w.repo}`} className="underline-offset-4 hover:text-foreground hover:underline">{w.repo}</Link>
                        <span className="text-muted-foreground/60"> @ </span>{w.ref}
                      </div>
                    </div>

                    <Owner owner={w.owner} />

                    <Status status={w.status} />

                    <span className="text-right text-caption text-muted-foreground">{w.active}</span>

                    <div className="flex items-center justify-end gap-1">
                      {w.status === "stopped" ? (
                        <Button variant="outline" size="sm" className="border-edge hover:border-edge-hover">Start</Button>
                      ) : (
                        <Button asChild variant="outline" size="sm" className="border-edge hover:border-edge-hover">
                          <a href="#" aria-label={`Open ${w.definition} on ${w.repo}`}>Open <ExternalLink /></a>
                        </Button>
                      )}
                      <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                          <Button variant="ghost" size="icon-sm" aria-label="More" className="text-muted-foreground"><MoreHorizontal /></Button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent align="end" className="w-44">
                          <DropdownMenuItem>Restart</DropdownMenuItem>
                          <DropdownMenuItem>Snapshot</DropdownMenuItem>
                          <DropdownMenuItem>Fork for an agent</DropdownMenuItem>
                          <DropdownMenuSeparator />
                          {w.status !== "stopped" && <DropdownMenuItem>Stop</DropdownMenuItem>}
                          <DropdownMenuItem variant="destructive">Delete</DropdownMenuItem>
                        </DropdownMenuContent>
                      </DropdownMenu>
                    </div>
                  </li>
                ))}
              </ul>
            </div>
          </section>

          <aside className="grid content-start gap-6">
            <section>
              <div className="flex items-baseline justify-between">
                <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">Definitions</h2>
                <Link href={`/${owner}/.workspaces`} className="inline-flex items-center gap-1 font-mono text-caption text-muted-foreground underline-offset-4 hover:text-foreground hover:underline">
                  .workspaces <ArrowRight className="size-3" />
                </Link>
              </div>
              <ul className="mt-3 divide-y divide-border border border-border">
                {WORKSPACE_DEFINITIONS.map((d) => (
                  <li key={d.name} className="flex items-center gap-3 px-4 py-2.5">
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        <span className="text-sm2 font-medium">{d.name}</span>
                        <Badge variant="outline" className="font-mono">{d.image}</Badge>
                      </div>
                      <Link href={`/${owner}/.workspaces/blob/${d.path}`} className="mt-0.5 inline-flex items-center gap-1.5 font-mono text-caption text-muted-foreground underline-offset-4 transition-colors hover:text-foreground hover:underline">
                        <FileCode className="size-3" />{d.path}
                      </Link>
                    </div>
                    <span className="text-caption text-muted-foreground">{d.sessions} live</span>
                  </li>
                ))}
              </ul>
              <p className="mt-2 text-caption text-muted-foreground">
                Change a file; the next session starts from it.
              </p>
            </section>
          </aside>
        </div>
      </main>
    </AppShell>
  );
}
