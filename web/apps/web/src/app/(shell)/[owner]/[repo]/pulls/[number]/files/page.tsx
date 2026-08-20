import { BackLink } from "@/components/repo/back-link";
import { PullHeader } from "@/components/repo/pull-page";
import { PullFiles } from "@/components/repo/pull-files";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";
import { pullData } from "../pull-data";

export default async function Page({
  params,
}: {
  params: Promise<{ owner: string; repo: string; number: string }>;
}) {
  const { owner, repo, number } = await params;
  const { token } = await guardRepo(owner, repo);
  const { pull, diff, counts } = await pullData(token, owner, repo, Number(number));

  return (
    <section className="min-w-0">
      <BackLink href={`/${owner}/${repo}/pulls`}>Pull requests</BackLink>
      <div className="mt-3">
        <PullHeader owner={owner} repo={repo} pull={pull} tab="files" counts={counts} stat={diff} />
      </div>
      <PullFiles base={`/${owner}/${repo}`} diff={diff} />
    </section>
  );
}
