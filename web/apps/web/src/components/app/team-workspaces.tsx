import Link from "next/link";
import { Bot, ExternalLink, GitFork, Layers, MoreHorizontal, Plus, Search, Split, Zap } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { Initials } from "@/components/app/initials";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { WORKSPACE_SESSIONS, type WorkspaceSession } from "@/lib/mock";
import type { Session } from "@/lib/session";

/** Who the workspace belongs to, drawn as its avatar. */
function OwnerMark({ owner }: { owner: WorkspaceSession["owner"] }) {
  if (owner.kind === "user") return <Initials name={owner.name} size={6} />;
  if (owner.kind === "agent") return <span className="flex size-6 shrink-0 items-center justify-center bg-primary/10 text-primary"><Bot className="size-3.5" /></span>;
  return <span className="flex size-6 shrink-0 items-center justify-center bg-muted text-muted-foreground" title="Owned by CI/CD"><Zap className="size-3.5" /></span>;
}

const ownerName = (o: WorkspaceSession["owner"]) => (o.kind === "user" ? o.login : o.kind === "agent" ? o.name : "ci");

/** A workspace: what it is, whose it is, whether it is running. Nothing else on the
 *  list — everything else is one click in. A flat list; where one was forked from
 *  is a fact on the row, not a shape of the page. */
export function TeamWorkspaces({ session }: { session: NonNullable<Session> }) {
  const owner = session.user.owner;
  const byId = new Map(WORKSPACE_SESSIONS.map((w) => [w.id, w]));
  // A workspace is named owner/definition: many people run the same definition,
  // so the owner is the part that tells them apart, and it comes first.
  const label = (w: WorkspaceSession) => `${ownerName(w.owner)}/${w.definition}`;
  return (
    <AppShell session={session} active="Workspaces">
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <div className="flex items-center gap-3">
          <div className="relative w-64">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input placeholder="Filter workspaces" className="h-8 pl-8" aria-label="Filter workspaces" />
          </div>
          <Button className="ml-auto"><Plus />New workspace</Button>
        </div>

        <ul className="mt-5 divide-y divide-border border border-border">
          {WORKSPACE_SESSIONS.map((w) => (
            <li key={w.id} className="flex items-center gap-4 px-5 py-3.5">
              <span
                className={`size-1.5 shrink-0 ${w.status === "running" ? "bg-success" : w.status === "idle" ? "bg-warning" : "bg-muted-foreground/40"}`}
                aria-label={w.status}
              />
              <OwnerMark owner={w.owner} />
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm2">
                  <span className="font-medium">{ownerName(w.owner)}</span>
                  <span className="text-muted-foreground">/</span>
                  <span className="font-medium">{w.definition}</span>
                  {w.owner.kind === "agent" && <span className="text-caption text-muted-foreground"> agent for {w.owner.for}</span>}
                  {w.owner.kind === "ci" && (
                    <span className="text-caption text-muted-foreground">
                      {" "}<Link href={`/${owner}/ci`} className="underline-offset-4 hover:text-foreground hover:underline">{w.owner.trigger} #{w.owner.run}</Link>
                    </span>
                  )}
                  <span className="text-muted-foreground"> · </span>
                  <Link href={`/${owner}/${w.repo}`} className="underline-offset-4 hover:underline">{w.repo}</Link>
                  <span className="font-mono text-caption text-muted-foreground"> {w.ref}</span>
                </div>
                <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-caption text-muted-foreground">
                  <span>{w.status === "stopped" ? `stopped ${w.active}` : `active ${w.active}`}</span>
                  {w.forkedFrom && (
                    <span className="inline-flex items-center gap-1" title={`Cloned from ${label(byId.get(w.forkedFrom) ?? w)}, with its branch and state at the time`}>
                      <GitFork className="size-3" />forked from {byId.get(w.forkedFrom) ? label(byId.get(w.forkedFrom)!) : w.forkedFrom}
                    </span>
                  )}
                  {w.environment && (
                    <Link
                      href={`/${owner}/environments`}
                      className="inline-flex items-center gap-1 underline-offset-4 hover:text-foreground hover:underline"
                      title={`Connected to the ${w.environment} environment`}
                    >
                      <Layers className="size-3" />{w.environment}
                    </Link>
                  )}
                  {w.intercepts && w.intercepts.length > 0 && (
                    <span
                      className="inline-flex items-center gap-1 text-primary"
                      title={`Traffic for ${w.intercepts.join(", ")} in ${w.environment} is routed to this workspace`}
                    >
                      <Split className="size-3" />intercepting {w.intercepts.join(", ")}
                    </span>
                  )}
                </div>
              </div>
              {w.owner.kind === "ci" ? (
                <Button asChild variant="outline" size="sm" className="w-20 border-edge hover:border-edge-hover">
                  <Link href={`/${owner}/ci`}>Logs</Link>
                </Button>
              ) : w.status === "stopped" ? (
                <Button variant="outline" size="sm" className="w-20 border-edge hover:border-edge-hover">Start</Button>
              ) : (
                <Button asChild variant="outline" size="sm" className="w-20 border-edge hover:border-edge-hover">
                  <a href="#">Open <ExternalLink /></a>
                </Button>
              )}
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button variant="ghost" size="icon-sm" aria-label="More" className="text-muted-foreground"><MoreHorizontal /></Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="w-40">
                  <DropdownMenuItem>Restart</DropdownMenuItem>
                  <DropdownMenuItem>Fork for an agent</DropdownMenuItem>
                  <DropdownMenuSeparator />
                  {w.status !== "stopped" && <DropdownMenuItem>Stop</DropdownMenuItem>}
                  {w.owner.kind !== "ci" && <DropdownMenuItem variant="destructive">Delete</DropdownMenuItem>}
                </DropdownMenuContent>
              </DropdownMenu>
            </li>
          ))}
        </ul>
      </main>
    </AppShell>
  );
}
