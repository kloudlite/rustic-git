import Link from "next/link";
import { CircleCheck, CircleX, Loader2, Search } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { REPO, RUNS } from "@/lib/mock-repo";

function Status({ s }: { s: "passing" | "failing" | "running" }) {
  if (s === "passing") return <CircleCheck className="size-4 shrink-0 text-success" aria-label="Passed" />;
  if (s === "failing") return <CircleX className="size-4 shrink-0 text-destructive" aria-label="Failed" />;
  return <Loader2 className="size-4 shrink-0 animate-spin text-primary" aria-label="Running" />;
}

export function ActionsView({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  return (
    <section>
      <div className="flex flex-wrap items-center gap-3">
        <div className="relative w-full max-w-xs">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input placeholder="Filter runs" className="h-8 pl-8" aria-label="Filter runs" />
        </div>
        <Tabs defaultValue="all">
          <TabsList>
            <TabsTrigger value="all">All workflows</TabsTrigger>
            <TabsTrigger value="ci">ci</TabsTrigger>
            <TabsTrigger value="release">release</TabsTrigger>
          </TabsList>
        </Tabs>
      </div>

      <ul className="mt-5 divide-y divide-border border border-border">
        {RUNS.map((r) => (
          <li key={r.id} className="flex items-center gap-4 px-5 py-3.5">
            <Status s={r.status} />
            <div className="min-w-0 flex-1">
              <Link href={`${base}/actions/${r.id}`} className="block truncate text-sm2 font-medium underline-offset-4 hover:underline">
                {r.workflow} <span className="font-normal text-muted-foreground">#{r.id}</span>
              </Link>
              <p className="mt-1 flex flex-wrap items-center gap-x-2 text-caption text-muted-foreground">
                <span className="font-mono">{r.sha}</span>
                <span aria-hidden>·</span>
                <span className="font-mono">{r.branch}</span>
                <span aria-hidden>·</span>
                <span>{r.trigger}</span>
              </p>
            </div>
            <span className="hidden text-caption text-muted-foreground sm:inline">{r.duration}</span>
            <span className="w-20 shrink-0 text-right text-caption text-muted-foreground">{r.when}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}
