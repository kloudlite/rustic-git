import Link from "next/link";
import { CircleCheck, CircleDot, MessageSquare, Plus, Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { ISSUES, REPO } from "@/lib/mock-repo";

export function IssuesView({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  const open = ISSUES.filter((i) => i.state === "open");
  return (
    <section>
      <div className="flex flex-wrap items-center gap-3">
        <div className="relative w-full max-w-xs">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input placeholder="Filter issues" className="h-8 pl-8" aria-label="Filter issues" />
        </div>
        <Tabs defaultValue="open">
          <TabsList>
            <TabsTrigger value="open">{open.length} open</TabsTrigger>
            <TabsTrigger value="closed">{ISSUES.length - open.length} closed</TabsTrigger>
          </TabsList>
        </Tabs>
        <Button className="ml-auto"><Plus />New issue</Button>
      </div>

      <ul className="mt-5 divide-y divide-border border border-border">
        {ISSUES.map((i) => (
          <li key={i.number} className="flex items-start gap-3 px-5 py-3.5">
            {i.state === "open"
              ? <CircleDot className="mt-0.5 size-4 shrink-0 text-success" aria-label="Open" />
              : <CircleCheck className="mt-0.5 size-4 shrink-0 text-primary" aria-label="Closed" />}
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <Link href={`${base}/issues/${i.number}`} className="text-sm2 font-medium underline-offset-4 hover:underline">{i.title}</Link>
                {i.labels.map((l) => (
                  <span key={l} className="border border-border px-1.5 py-px text-micro font-medium text-muted-foreground">{l}</span>
                ))}
              </div>
              <p className="mt-1 text-caption text-muted-foreground">
                #{i.number} opened {i.when} by <span className="font-medium text-foreground/80">{i.author}</span>
              </p>
            </div>
            {i.comments > 0 && (
              <span className="flex shrink-0 items-center gap-1 text-caption text-muted-foreground">
                <MessageSquare className="size-3.5" /> {i.comments}
              </span>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
