import Link from "next/link";
import { Bot, CornerDownRight, ExternalLink, Layers, MoreHorizontal, Plus, Search, Split } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { Initials } from "@/components/app/initials";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { WORKSPACE_SESSIONS, type WorkspaceSession } from "@/lib/mock";
import type { Session } from "@/lib/session";

function Owner({ owner }: { owner: WorkspaceSession["owner"] }) {
  if (owner.kind === "user") return <><Initials name={owner.name} size={6} />{owner.login}</>;
  return (
    <>
      <span className="flex size-6 shrink-0 items-center justify-center bg-primary/10 text-primary"><Bot className="size-3.5" /></span>
      {owner.name}
      <span className="text-muted-foreground">· for {owner.for}</span>
    </>
  );
}

/** A workspace: what it is, whose it is, whether it is running. Nothing else on the
 *  list — everything else is one click in.
 *
 *  Workspaces an agent cloned from a person's sit directly under that person's, so
 *  the lineage is the layout: yours, then the ones working beside it. */
export function TeamWorkspaces({ session }: { session: NonNullable<Session> }) {
  const owner = session.user.owner;
  const roots = WORKSPACE_SESSIONS.filter((w) => !w.forkedFrom);
  const forksOf = (id: string) => WORKSPACE_SESSIONS.filter((w) => w.forkedFrom === id);
  const ordered = roots.flatMap((r) => [r, ...forksOf(r.id)]);
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
          {ordered.map((w) => (
            <li key={w.id} className={`flex items-center gap-4 py-3.5 pr-5 ${w.forkedFrom ? "bg-muted/30 pl-9" : "pl-5"}`}>
              {w.forkedFrom
                ? <CornerDownRight className="size-3.5 shrink-0 text-muted-foreground/60" aria-label="Forked from the workspace above" />
                : <span
                    className={`size-1.5 shrink-0 ${w.status === "running" ? "bg-success" : w.status === "idle" ? "bg-warning" : "bg-muted-foreground/40"}`}
                    aria-label={w.status}
                  />}
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm2 font-medium">
                  {w.forkedFrom && (
                    <span
                      className={`mr-2 inline-block size-1.5 align-middle ${w.status === "running" ? "bg-success" : w.status === "idle" ? "bg-warning" : "bg-muted-foreground/40"}`}
                      aria-label={w.status}
                    />
                  )}
                  {w.definition}
                  <span className="font-normal text-muted-foreground"> · </span>
                  <Link href={`/${owner}/${w.repo}`} className="font-normal underline-offset-4 hover:underline">{w.repo}</Link>
                  <span className="font-mono text-caption font-normal text-muted-foreground"> {w.ref}</span>
                </div>
                <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-caption text-muted-foreground">
                  <span>{w.status === "stopped" ? `stopped ${w.active}` : `active ${w.active}`}</span>
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
              <span className="hidden w-56 items-center gap-2 text-sm2 sm:flex"><Owner owner={w.owner} /></span>
              {w.status === "stopped" ? (
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
                  <DropdownMenuItem variant="destructive">Delete</DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </li>
          ))}
        </ul>
      </main>
    </AppShell>
  );
}
