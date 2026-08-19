import { CircleCheck, CircleX, Loader2, Zap } from "lucide-react";
import { DeclaredPage, Source } from "@/components/app/declared-list";
import { TRIGGERS } from "@/lib/mock";
import type { Session } from "@/lib/session";

function Status({ s }: { s: "passing" | "failing" | "running" }) {
  if (s === "passing") return <CircleCheck className="size-4 shrink-0 text-success" aria-label="Last run passed" />;
  if (s === "failing") return <CircleX className="size-4 shrink-0 text-destructive" aria-label="Last run failed" />;
  return <Loader2 className="size-4 shrink-0 animate-spin text-primary" aria-label="Running" />;
}

export function TeamTriggers({ session }: { session: NonNullable<Session> }) {
  const owner = session.user.owner;
  return (
    <DeclaredPage session={session} active="CI Triggers" filterLabel="Filter triggers" dir="actions" count={TRIGGERS.length}>
      <ul className="divide-y divide-border border border-border bg-card">
        {TRIGGERS.map((t) => (
          <li key={`${t.source.repo}/${t.source.path}`} className="flex items-center gap-4 px-5 py-3.5">
            <Status s={t.last.status} />
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-x-2 text-sm2">
                <span className="font-medium">{t.name}</span>
                <span className="inline-flex items-center gap-1 text-caption text-muted-foreground"><Zap className="size-3.5" />on {t.on}</span>
              </div>
              <div className="mt-1"><Source owner={owner} source={t.source} /></div>
            </div>
            <span className="hidden text-caption text-muted-foreground sm:inline">{t.last.duration}</span>
            <span className="w-16 text-right text-caption text-muted-foreground">{t.last.when}</span>
          </li>
        ))}
      </ul>
    </DeclaredPage>
  );
}
