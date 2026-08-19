"use client";

import { useMemo, useState } from "react";
import Link from "next/link";
import { Lock, Globe, Plus, Search, Settings2, SquareCode } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { ApiRepo } from "@/lib/api";

/** `.kloudlite` holds how the team is configured, so it is drawn as what it is
 *  rather than as an ordinary repo. */
function RepoIcon({ system }: { system: boolean }) {
  const cls = "size-4 shrink-0";
  return system
    ? <Settings2 className={`${cls} text-primary`} aria-label="Team configuration repo" />
    : <SquareCode className={`${cls} text-muted-foreground`} />;
}

function when(ms: number) {
  const d = Math.floor((Date.now() - ms) / 1000);
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  if (d < 2592000) return `${Math.floor(d / 86400)}d ago`;
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}

/** The owner's repos, with the filter and the scope tabs doing what they look
 *  like they do. Filtering is local: the whole list is already here, so a round
 *  trip per keystroke would be slower and no more correct.
 *
 *  Every row is the same height whether or not it has a description — the second
 *  line always exists, because a list whose rows change height as you read down
 *  it reads as broken rather than as sparse. */
export function RepoList({ owner, repos }: { owner: string; repos: ApiRepo[] }) {
  const [q, setQ] = useState("");
  const [scope, setScope] = useState<"all" | "public" | "private">("all");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    return repos.filter((r) => {
      if (scope === "public" && !r.public) return false;
      if (scope === "private" && r.public) return false;
      if (!needle) return true;
      return r.name.toLowerCase().includes(needle) || r.description.toLowerCase().includes(needle);
    });
  }, [repos, q, scope]);

  const counts = {
    all: repos.length,
    public: repos.filter((r) => r.public).length,
    private: repos.filter((r) => !r.public).length,
  };

  return (
    <>
      <div className="flex flex-wrap items-center gap-3">
        <div className="relative w-full max-w-xs">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Filter repos"
            aria-label="Filter repos"
            className="h-8 pl-8 text-sm2"
          />
        </div>
        <Tabs value={scope} onValueChange={(v) => setScope(v as typeof scope)}>
          <TabsList className="h-8">
            {(["all", "public", "private"] as const).map((s) => (
              <TabsTrigger key={s} value={s} className="text-sm2 capitalize">
                {s}
                <span className="ml-1.5 text-muted-foreground">{counts[s]}</span>
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>
        <Button asChild className="ml-auto">
          <Link href={`/new-repo?owner=${owner}`}><Plus />New repo</Link>
        </Button>
      </div>

      {repos.length === 0 ? (
        <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
          <p className="text-sm2 font-medium">No repos yet</p>
          <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
            Create one and push to it, or add it as a remote to something you already have.
          </p>
          <Button asChild className="mt-5">
            <Link href={`/new-repo?owner=${owner}`}><Plus />New repo</Link>
          </Button>
        </div>
      ) : shown.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          Nothing matches that.
        </p>
      ) : (
        <ul className="mt-5 divide-y divide-border border border-border bg-card">
          {shown.map((r) => (
            <li key={r._id}>
              <Link
                href={`/${r.owner}/${r.name}`}
                className="flex items-start gap-4 px-5 py-4 transition-colors hover:bg-muted/50"
              >
                <span className="mt-0.5"><RepoIcon system={r.name === ".kloudlite"} /></span>
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-2.5">
                    <span className="truncate text-body font-medium">{r.name}</span>
                    <span className="flex shrink-0 items-center gap-1 border border-border px-1.5 py-0.5 text-micro font-medium text-muted-foreground">
                      {r.public ? <Globe className="size-3" /> : <Lock className="size-3" />}
                      {r.public ? "Public" : "Private"}
                    </span>
                  </span>
                  {/* Always rendered. An absent description says something true —
                      that nobody has written one — and keeps the row its own height. */}
                  <span className={`mt-1 block truncate text-sm2 ${r.description ? "text-muted-foreground" : "text-muted-foreground/50 italic"}`}>
                    {r.description || "No description"}
                  </span>
                </span>
                {/* Only when the api actually sent a number. Web and api do not
                    flip at the same instant during a rollout, and an older api
                    sends a BSON date, which would render as "Invalid Date". */}
                {typeof r.createdAt === "number" && (
                  <span className="shrink-0 text-caption text-muted-foreground" title={new Date(r.createdAt).toISOString()}>
                    Created {when(r.createdAt)}
                  </span>
                )}
              </Link>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
