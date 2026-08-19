import { redirect } from "next/navigation";
import { NewPullForm } from "@/components/repo/new-pull-form";
import { guardRepo } from "@/app/[owner]/[repo]/guard";
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
  // Nothing to propose between: a repo with one branch has no second side.
  if (branches.length < 2) redirect(`/${owner}/${repo}/pulls`);

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
