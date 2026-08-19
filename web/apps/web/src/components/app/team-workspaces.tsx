import { Bot, Play, Square } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Initials } from "@/components/app/initials";
import { DeclaredPage, Source } from "@/components/app/declared-list";
import { WORKSPACES } from "@/lib/mock";
import type { Session } from "@/lib/session";

export function TeamWorkspaces({ session }: { session: NonNullable<Session> }) {
  const owner = session.user.owner;
  return (
    <DeclaredPage session={session} active="Workspaces" filterLabel="Filter workspaces" dir=".workspaces" count={WORKSPACES.length}>
      <ul className="divide-y divide-border border border-border">
        {WORKSPACES.map((w) => (
          <li key={`${w.source.repo}/${w.name}`} className="flex items-center gap-4 px-5 py-3.5">
            <span className={`size-1.5 shrink-0 ${w.status === "running" ? "bg-success" : "bg-muted-foreground/40"}`} aria-hidden />
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2 text-sm2">
                <span className="font-medium">{w.name}</span>
                <Badge variant="outline" className="font-mono">{w.image}</Badge>
                {w.agents > 0 && (
                  <span className="inline-flex items-center gap-1 text-caption text-muted-foreground"><Bot className="size-3.5" />{w.agents} agents</span>
                )}
              </div>
              <div className="mt-1"><Source owner={owner} source={w.source} /></div>
            </div>
            <div className="hidden items-center -space-x-1 sm:flex">
              {w.users.map((u) => <Initials key={u} name={u} size={6} className="ring-2 ring-background" />)}
            </div>
            <span className="w-16 text-right text-caption text-muted-foreground">{w.updated}</span>
            <Button variant="outline" size="sm" className="border-edge hover:border-edge-hover">
              {w.status === "running" ? <><Square />Stop</> : <><Play />Open</>}
            </Button>
          </li>
        ))}
      </ul>
    </DeclaredPage>
  );
}
