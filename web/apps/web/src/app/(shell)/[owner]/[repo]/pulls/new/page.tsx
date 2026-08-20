import Link from "next/link";
import { GitBranch } from "lucide-react";
import { Button } from "@/components/ui/button";
import { NewPullForm } from "@/components/repo/new-pull-form";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";
import { defaultBranch, refs, shortRef } from "@/lib/browse";

export default async function Page({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string; repo: string }>;
  searchParams: Promise<{ base?: string; head?: string }>;
}) {
  const { owner, repo } = await params;
  const { token } = await guardRepo(owner, repo);
  const { base, head } = await searchParams;

  const all = await refs(token, owner, repo);
  if (!all.ok) throw new Error(all.message);
  const branches = all.value.filter((r) => r.kind === "branch").map((r) => shortRef(r.name));
  // Nothing to propose between: a repo with one branch has no second side. Say
  // so rather than redirecting — bouncing silently back to the list is what a
  // broken link looks like, and the reader is left thinking the page is missing.
  if (branches.length < 2) {
    return (
      <section className="min-w-0">
        <h1 className="text-title font-semibold tracking-title">New pull request</h1>
        <div className="mt-6 border border-border bg-card px-5 py-14 text-center">
          <GitBranch className="mx-auto size-5 text-muted-foreground" />
          <p className="mt-3 text-sm2 font-medium">
            {branches.length === 1 ? `Only one branch` : `No branches yet`}
          </p>
          <p className="mx-auto mt-1 max-w-sm text-sm2 text-muted-foreground">
            A pull request proposes one branch onto another, so this repository
            needs a second one. Push a branch and it will show up here.
          </p>
          <Button asChild variant="outline" className="mt-5">
            <Link href={`/${owner}/${repo}/pulls`}>Back to pull requests</Link>
          </Button>
        </div>
      </section>
    );
  }

  const fallback = defaultBranch(all.value);
  return (
    <NewPullForm
      owner={owner}
      repo={repo}
      branches={branches}
      defaultBase={base && branches.includes(base) ? base : (fallback ? shortRef(fallback.name) : branches[0])}
      defaultHead={head && branches.includes(head) ? head : branches.find((b) => b !== (fallback ? shortRef(fallback.name) : branches[0])) ?? branches[1]}
    />
  );
}
