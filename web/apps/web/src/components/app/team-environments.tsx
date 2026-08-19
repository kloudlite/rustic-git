import { Bot, CornerDownRight, MoreHorizontal, Plus, Search } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { Initials } from "@/components/app/initials";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { TEAM_ENVIRONMENTS, type Environment } from "@/lib/mock";
import type { Session } from "@/lib/session";

function Owner({ owner }: { owner: Environment["owner"] }) {
  if (owner.kind === "team") return <><Badge variant="outline">team</Badge><span className="text-muted-foreground">shared baseline</span></>;
  if (owner.kind === "user") return <><Initials name={owner.name} size={6} />{owner.login}</>;
  return (
    <>
      <span className="flex size-6 shrink-0 items-center justify-center bg-primary/10 text-primary"><Bot className="size-3.5" /></span>
      {owner.name}
      <span className="text-muted-foreground">· for {owner.for}</span>
    </>
  );
}

/** Environments are developers' working environments: a shared baseline, and the
 *  forks people and their agents work in. Forks nest under what they were forked
 *  from, so the lineage is the layout. */
export function TeamEnvironments({ session }: { session: NonNullable<Session> }) {
  const depth = (e: Environment): number => (e.forkedFrom ? 1 + depth(TEAM_ENVIRONMENTS.find((x) => x.name === e.forkedFrom)!) : 0);
  const ordered: Environment[] = [];
  const walk = (parent?: string) => {
    for (const e of TEAM_ENVIRONMENTS.filter((x) => x.forkedFrom === parent)) { ordered.push(e); walk(e.name); }
  };
  walk(undefined);

  return (
    <AppShell session={session} active="Environments">
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <div className="flex items-center gap-3">
          <div className="relative w-64">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input placeholder="Filter environments" className="h-8 pl-8" aria-label="Filter environments" />
          </div>
          <Button className="ml-auto"><Plus />Fork environment</Button>
        </div>

        <ul className="mt-5 divide-y divide-border border border-border">
          {ordered.map((e) => {
            const d = depth(e);
            return (
              <li
                key={e.name}
                className={`flex items-center gap-4 py-3.5 pr-5 ${d > 0 ? "bg-muted/30" : ""}`}
                style={{ paddingLeft: `calc(var(--spacing) * ${5 + d * 4})` }}
              >
                {d > 0
                  ? <CornerDownRight className="size-3.5 shrink-0 text-muted-foreground/60" aria-label={`Forked from ${e.forkedFrom}`} />
                  : <span className={`size-1.5 shrink-0 ${e.healthy ? "bg-success" : "bg-destructive"}`} aria-label={e.healthy ? "healthy" : "unhealthy"} />}
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm2 font-medium">
                    {d > 0 && <span className={`mr-2 inline-block size-1.5 align-middle ${e.healthy ? "bg-success" : "bg-destructive"}`} aria-label={e.healthy ? "healthy" : "unhealthy"} />}
                    {e.name}
                  </div>
                  <div className="mt-0.5 text-caption text-muted-foreground">
                    {e.services} services · {e.forkedFrom ? `forked from ${e.forkedFrom}` : "baseline"} · updated {e.when}
                  </div>
                </div>
                <span className="hidden w-56 items-center gap-2 text-sm2 sm:flex"><Owner owner={e.owner} /></span>
                <Button variant="outline" size="sm" className="w-20 border-edge hover:border-edge-hover">Switch</Button>
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button variant="ghost" size="icon-sm" aria-label="More" className="text-muted-foreground"><MoreHorizontal /></Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end" className="w-40">
                    <DropdownMenuItem>Fork</DropdownMenuItem>
                    <DropdownMenuItem>Snapshot</DropdownMenuItem>
                    <DropdownMenuItem>Reset to {e.forkedFrom ?? "definition"}</DropdownMenuItem>
                    <DropdownMenuSeparator />
                    {e.owner.kind !== "team" && <DropdownMenuItem variant="destructive">Delete</DropdownMenuItem>}
                  </DropdownMenuContent>
                </DropdownMenu>
              </li>
            );
          })}
        </ul>
      </main>
    </AppShell>
  );
}
