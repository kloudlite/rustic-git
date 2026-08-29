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
  const { pull, diff } = await pullData(token, owner, repo, number);

  // The pull's head branch: the diff is what that branch brings, so that is where
  // its files can actually be read.
  return <PullFiles base={`/${owner}/${repo}`} diff={diff} refName={pull.head} />;
}
