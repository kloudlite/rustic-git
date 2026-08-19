"use client";

import { useMemo, useState } from "react";
import Link from "next/link";
import { Bot, ExternalLink, MoreHorizontal, Plus, Search, Split, Zap } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { Initials } from "@/components/app/initials";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { WORKSPACE_SESSIONS, type WorkspaceSession } from "@/lib/mock";
import type { Session } from "@/lib/session";

/** Who the workspace belongs to, drawn as its avatar. */
function OwnerMark({ owner }: { owner: WorkspaceSession["owner"] }) {
  if (owner.kind === "user") return <Initials name={owner.name} size={6} />;
  if (owner.kind === "agent") return <span className="flex size-6 shrink-0 items-center justify-center bg-primary/10 text-primary"><Bot className="size-3.5" /></span>;
  return <span className="flex size-6 shrink-0 items-center justify-center bg-muted text-muted-foreground" title="Owned by the system, for CI/CD"><Zap className="size-3.5" /></span>;
}

const ownerName = (o: WorkspaceSession["owner"]) => (o.kind === "user" ? o.login : o.kind === "agent" ? o.name : "system");

/** A workspace: what it is, whose it is, whether it is running. Nothing else on the
 *  list — everything else is one click in. A flat list; where one was forked from
 *  is a fact on the row, not a shape of the page. */
export function TeamWorkspaces({ session }: { session: NonNullable<Session> }) {
  const owner = session.user.owner;
  const me = session.user.owner;
  const [scope, setScope] = useState<"all" | "mine">("all");
  const [q, setQ] = useState("");

  // "Yours" is what you own and what works for you: your workspaces, and your agents'.
  const isMine = (w: WorkspaceSession) =>
    (w.owner.kind === "user" && w.owner.login === me) || (w.owner.kind === "agent" && w.owner.for === me);
  const visible = useMemo(() => {
    const needle = q.trim().toLowerCase();
    return WORKSPACE_SESSIONS.filter((w) => (scope === "all" || isMine(w)))
      .filter((w) => !needle || `${ownerName(w.owner)}/${w.definition} ${w.repo} ${w.ref} ${w.environment ?? ""}`.toLowerCase().includes(needle));
  }, [scope, q]);
  const mineCount = WORKSPACE_SESSIONS.filter(isMine).length;
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
            <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Filter workspaces" className="h-8 pl-8" aria-label="Filter workspaces" />
          </div>
          <Tabs value={scope} onValueChange={(v) => setScope(v as "all" | "mine")}>
            <TabsList>
              <TabsTrigger value="all">All <span className="text-muted-foreground">{WORKSPACE_SESSIONS.length}</span></TabsTrigger>
              <TabsTrigger value="mine">Yours <span className="text-muted-foreground">{mineCount}</span></TabsTrigger>
            </TabsList>
          </Tabs>
          <Button className="ml-auto"><Plus />New workspace</Button>
        </div>

        <div className="mt-5 border border-border">
          <div className="grid grid-cols-workspaces items-center gap-4 border-b border-border bg-muted/40 px-5 py-2 text-micro font-semibold uppercase tracking-label text-muted-foreground">
            <span>Workspace</span>
            <span>Code</span>
            <span>Environment</span>
            <span>Active</span>
            <span />
          </div>
          <ul className="divide-y divide-border">
            {visible.length === 0 && (
              <li className="px-5 py-8 text-center text-sm2 text-muted-foreground">No workspaces match.</li>
            )}
            {visible.map((w) => (
              <li key={w.id} className="grid grid-cols-workspaces items-center gap-4 px-5 py-3">
                {/* Workspace: who/what, and where it came from */}
                <div className="flex min-w-0 items-center gap-3">
                  <OwnerMark owner={w.owner} />
                  <div className="min-w-0">
                    <div className="truncate text-sm2 font-medium">
                      {ownerName(w.owner)}<span className="text-muted-foreground">/</span>{w.definition}
                    </div>
                    <div className="truncate text-caption text-muted-foreground">
                      {w.owner.kind === "agent" && `agent for ${w.owner.for}`}
                      {w.owner.kind === "ci" && <Link href={`/${owner}/ci`} className="underline-offset-4 hover:text-foreground hover:underline">{w.owner.trigger} #{w.owner.run}</Link>}
                      {w.owner.kind === "user" && (w.forkedFrom ? "" : "\u00a0")}
                      {w.forkedFrom && (
                        <span title={`Cloned from ${label(byId.get(w.forkedFrom) ?? w)}, with its branch and state at the time`}>
                          {w.owner.kind !== "user" && " · "}forked from {byId.get(w.forkedFrom) ? label(byId.get(w.forkedFrom)!) : w.forkedFrom}
                        </span>
                      )}
                    </div>
                  </div>
                </div>

                {/* Code: repo and branch */}
                <div className="min-w-0">
                  <Link href={`/${owner}/${w.repo}`} className="block truncate text-sm2 underline-offset-4 hover:underline">{w.repo}</Link>
                  <div className="truncate font-mono text-caption text-muted-foreground">{w.ref}</div>
                </div>

                {/* Environment, and what it intercepts there */}
                <div className="min-w-0">
                  {w.environment ? (
                    <>
                      <Link href={`/${owner}/environments`} className="block truncate text-sm2 underline-offset-4 hover:underline">{w.environment}</Link>
                      {w.intercepts && w.intercepts.length > 0 && (
                        <div className="flex items-center gap-1 truncate text-caption text-primary" title={`Traffic for ${w.intercepts.join(", ")} in ${w.environment} is routed to this workspace`}>
                          <Split className="size-3 shrink-0" />intercepting {w.intercepts.join(", ")}
                        </div>
                      )}
                    </>
                  ) : (
                    <span className="text-sm2 text-muted-foreground">—</span>
                  )}
                </div>

                {/* Active */}
                <div className="flex items-center gap-2 text-caption text-muted-foreground">
                  <span
                    className={`size-1.5 shrink-0 ${w.status === "running" ? "bg-success" : w.status === "idle" ? "bg-warning" : "bg-muted-foreground/40"}`}
                    aria-label={w.status}
                  />
                  {w.status === "stopped" ? `stopped ${w.active}` : w.active}
                </div>

                <div className="flex items-center justify-end gap-1">
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
                </div>
              </li>
            ))}
          </ul>
        </div>
      </main>
    </AppShell>
  );
}
