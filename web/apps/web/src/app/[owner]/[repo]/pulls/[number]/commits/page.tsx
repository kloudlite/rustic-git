import { BackLink } from "@/components/repo/back-link";
import { PullHeader } from "@/components/repo/pull-page";
import { PullCommits } from "@/components/repo/pull-commits";
import { guardRepo } from "@/app/[owner]/[repo]/guard";
import { pullData } from "../pull-data";

export default async function Page({
  params,
}: {
  params: Promise<{ owner: string; repo: string; number: string }>;
}) {
  const { owner, repo, number } = await params;
  const { token } = await guardRepo(owner, repo);
  const { pull, comparison, counts } = await pullData(token, owner, repo, Number(number));

  return (
    <section className="min-w-0">
      <BackLink href={`/${owner}/${repo}/pulls`}>Pull requests</BackLink>
      <div className="mt-3">
        <PullHeader owner={owner} repo={repo} pull={pull} tab="commits" counts={counts} />
      </div>
      <PullCommits owner={owner} repo={repo} comparison={comparison} />
    </section>
  );
}
