import Link from "next/link";
import { GitPullRequest, Plus } from "lucide-react";
import { Button } from "@/components/ui/button";
import { StateBadge } from "@/components/repo/pull-state";
import { listPulls } from "@/lib/api";

/** Every proposed change, newest first. State is on the row rather than behind a
 *  filter tab: a short list is read, not queried. */
export async function PullsView({ token, owner, repo }: { token: string; owner: string; repo: string }) {
  const base = `/${owner}/${repo}`;
  const pulls = await listPulls(token, owner, repo);
  if (!pulls.ok) throw new Error(pulls.message);

  return (
    <section className="min-w-0">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="text-title font-semibold tracking-title">Pull requests</h1>
        <Button asChild className="ml-auto">
          <Link href={`${base}/pulls/new`}><Plus />New pull request</Link>
        </Button>
      </div>

      {pulls.value.length === 0 ? (
        <div className="mt-6 border border-border bg-card px-5 py-14 text-center">
          <p className="text-sm2 font-medium">No pull requests</p>
          <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
            Push a branch, then open one to get it reviewed and onto the base branch.
          </p>
          <Button asChild className="mt-5">
            <Link href={`${base}/pulls/new`}><Plus />New pull request</Link>
          </Button>
        </div>
      ) : (
        <ul className="mt-6 divide-y divide-border border border-border bg-card">
          {pulls.value.map((p) => (
            <li key={p._id}>
              <Link href={`${base}/pulls/${p.number}`} className="flex items-start gap-4 px-5 py-4 transition-colors hover:bg-muted/50">
                <GitPullRequest className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-2.5">
                    <span className="truncate text-body font-medium">{p.title}</span>
                    <StateBadge state={p.state} />
                  </span>
                  <span className="mt-1 block truncate text-sm2 text-muted-foreground">
                    #{p.number} · <span className="font-medium text-foreground/80">{p.author}</span> wants to merge{" "}
                    <code className="font-mono text-caption">{p.head}</code> into{" "}
                    <code className="font-mono text-caption">{p.base}</code>
                  </span>
                </span>
                {p.comments.length > 0 && (
                  <span className="shrink-0 text-caption text-muted-foreground">
                    {p.comments.length} {p.comments.length === 1 ? "comment" : "comments"}
                  </span>
                )}
              </Link>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
