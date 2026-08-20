import { BackLink } from "@/components/repo/back-link";
import { PullHeader } from "@/components/repo/pull-page";
import { PullConversation } from "@/components/repo/pull-conversation";
import { guardRepo } from "@/app/[owner]/[repo]/guard";
import { pullData } from "./pull-data";

export default async function Page({
  params,
}: {
  params: Promise<{ owner: string; repo: string; number: string }>;
}) {
  const { owner, repo, number } = await params;
  const { token } = await guardRepo(owner, repo);
  const { pull, counts, diff } = await pullData(token, owner, repo, Number(number));

  return (
    <section className="min-w-0">
      <BackLink href={`/${owner}/${repo}/pulls`}>Pull requests</BackLink>
      <div className="mt-3">
        <PullHeader owner={owner} repo={repo} pull={pull} tab="conversation" counts={counts} stat={diff} />
      </div>
      <PullConversation owner={owner} repo={repo} pull={pull} />
    </section>
  );
}
