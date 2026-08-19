import Link from "next/link";
import { CircleCheck, CircleX, GitMerge, GitPullRequest, Loader2, Plus, Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { PULLS, REPO } from "@/lib/mock-repo";

function Checks({ state }: { state: "passing" | "failing" | "pending" }) {
  if (state === "passing") return <CircleCheck className="size-4 text-success" aria-label="Checks passing" />;
  if (state === "failing") return <CircleX className="size-4 text-destructive" aria-label="Checks failing" />;
  return <Loader2 className="size-4 animate-spin text-muted-foreground" aria-label="Checks running" />;
}

export function PullsView({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  const open = PULLS.filter((p) => p.state === "open");
  return (
    <section>
      <div className="flex flex-wrap items-center gap-3">
        <div className="relative w-full max-w-xs">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input placeholder="Filter pull requests" className="h-8 pl-8" aria-label="Filter pull requests" />
        </div>
        <Tabs defaultValue="open">
          <TabsList>
            <TabsTrigger value="open">{open.length} open</TabsTrigger>
            <TabsTrigger value="merged">{PULLS.length - open.length} merged</TabsTrigger>
          </TabsList>
        </Tabs>
        <Button className="ml-auto"><Plus />New pull request</Button>
      </div>

      <ul className="mt-5 divide-y divide-border border border-border">
        {PULLS.map((p) => (
          <li key={p.number} className="flex items-start gap-3 px-5 py-3.5">
            {p.state === "open"
              ? <GitPullRequest className="mt-0.5 size-4 shrink-0 text-success" aria-label="Open" />
              : <GitMerge className="mt-0.5 size-4 shrink-0 text-primary" aria-label="Merged" />}
            <div className="min-w-0 flex-1">
              <Link href={`${base}/pulls/${p.number}`} className="text-sm2 font-medium underline-offset-4 hover:underline">{p.title}</Link>
              <p className="mt-1 flex flex-wrap items-center gap-x-2 text-caption text-muted-foreground">
                <span>#{p.number} opened {p.when} by <span className="font-medium text-foreground/80">{p.author}</span></span>
                <span aria-hidden>·</span>
                <span className="font-mono">{p.branch}</span>
                {p.reviews > 0 && <><span aria-hidden>·</span><span>{p.reviews} review{p.reviews > 1 ? "s" : ""}</span></>}
              </p>
            </div>
            <Checks state={p.checks} />
          </li>
        ))}
      </ul>
    </section>
  );
}
