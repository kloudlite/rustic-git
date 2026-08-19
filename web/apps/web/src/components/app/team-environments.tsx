import { ExternalLink, GitBranch } from "lucide-react";
import { Button } from "@/components/ui/button";
import { DeclaredPage, Source } from "@/components/app/declared-list";
import { TEAM_ENVIRONMENTS } from "@/lib/mock";
import type { Session } from "@/lib/session";

export function TeamEnvironments({ session }: { session: NonNullable<Session> }) {
  const owner = session.user.owner;
  return (
    <DeclaredPage session={session} active="Environments" filterLabel="Filter environments" dir=".environments" count={TEAM_ENVIRONMENTS.length}>
      <ul className="divide-y divide-border border border-border">
        {TEAM_ENVIRONMENTS.map((e) => (
          <li key={`${e.source.repo}/${e.name}`} className="flex items-center gap-4 px-5 py-3.5">
            <span className={`size-1.5 shrink-0 ${e.healthy ? "bg-success" : "bg-destructive"}`} aria-hidden />
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-x-2 text-sm2">
                <span className="font-medium">{e.name}</span>
                <span className="inline-flex items-center gap-1 text-caption text-muted-foreground"><GitBranch className="size-3.5" />tracks {e.tracks}</span>
              </div>
              <div className="mt-1"><Source owner={owner} source={e.source} /></div>
            </div>
            <span className="font-mono text-caption text-muted-foreground">{e.sha}</span>
            <span className="w-16 text-right text-caption text-muted-foreground">{e.when}</span>
            {e.url
              ? <Button asChild variant="outline" size="sm" className="border-edge hover:border-edge-hover"><a href={e.url} target="_blank" rel="noreferrer">Open <ExternalLink /></a></Button>
              : <span className="w-16" />}
          </li>
        ))}
      </ul>
    </DeclaredPage>
  );
}
