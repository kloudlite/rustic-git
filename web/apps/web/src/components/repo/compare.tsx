import Link from "next/link";
import { ArrowLeft, ChevronDown, GitBranch, GitPullRequest } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldLabel } from "@/components/auth/auth-card";
import { PULL, REPO } from "@/lib/mock-repo";

function BranchButton({ label, value }: { label: string; value: string }) {
  return (
    <button type="button" className="flex h-8 items-center gap-2 border border-edge px-2.5 text-sm2 transition-colors hover:bg-muted">
      <span className="text-muted-foreground">{label}:</span>
      <GitBranch className="size-3.5 text-muted-foreground" />
      <span className="font-medium">{value}</span>
      <ChevronDown className="size-3.5 text-muted-foreground" />
    </button>
  );
}

/** Opening a pull request. Pick base and head, see what would land, then name it.
 *  The comparison is above the form because the form is only worth filling once
 *  the comparison shows something. */
export function CompareView({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  return (
    <section>
      <h1 className="text-title font-semibold tracking-title">Open a pull request</h1>
      <p className="mt-1 text-sm2 text-muted-foreground">Choose two branches to see what changed, then describe it.</p>

      <div className="mt-6 flex flex-wrap items-center gap-2 border border-border bg-muted/40 px-4 py-3">
        <BranchButton label="base" value={PULL.base} />
        <ArrowLeft className="size-4 text-muted-foreground" />
        <BranchButton label="compare" value={PULL.head} />
        <span className="ml-auto text-sm2 text-muted-foreground">
          Able to merge · {PULL.commits.length} commits · {PULL.stats.files} files ·{" "}
          <span className="text-success">+{PULL.stats.additions}</span> <span className="text-destructive">−{PULL.stats.deletions}</span>
        </span>
      </div>

      <div className="mt-8 grid gap-10 lg:grid-cols-overview">
        <form className="grid gap-5">
          <div className="grid gap-2">
            <FieldLabel htmlFor="title">Title</FieldLabel>
            <Input id="title" name="title" defaultValue={PULL.commits[0].message} className="h-9" />
          </div>
          <div className="grid gap-2">
            <FieldLabel htmlFor="body">Description</FieldLabel>
            <textarea id="body" name="body" rows={8} placeholder="What changed, and why. Reference issues with #." className="block w-full resize-y border border-input bg-transparent px-3 py-2 text-sm2 outline-none placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50" />
          </div>
          <div className="flex items-center gap-3">
            <Button type="submit"><GitPullRequest />Create pull request</Button>
            <Link href={`${base}/pulls`} className="text-sm2 text-muted-foreground hover:text-foreground">Cancel</Link>
          </div>
        </form>

        <aside>
          <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">Commits</h2>
          <ul className="mt-3 divide-y divide-border border border-border">
            {[...PULL.commits].reverse().map((c) => (
              <li key={c.sha} className="flex items-center gap-3 px-4 py-2.5">
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm2">{c.message}</div>
                  <div className="text-caption text-muted-foreground">{c.author} · {c.when}</div>
                </div>
                <Link href={`${base}/commit/${c.sha}`} className="font-mono text-caption text-primary underline-offset-4 hover:underline">{c.sha}</Link>
              </li>
            ))}
          </ul>
        </aside>
      </div>
    </section>
  );
}
