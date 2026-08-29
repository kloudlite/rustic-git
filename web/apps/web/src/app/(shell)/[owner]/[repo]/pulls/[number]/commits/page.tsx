import { PullCommits } from "@/components/repo/pull-commits";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";
import { pullData } from "../pull-data";

export default async function Page({
  params,
}: {
  params: Promise<{ owner: string; repo: string; number: string }>;
}) {
  const { owner, repo, number } = await params;
  const { token } = await guardRepo(owner, repo);
  const { comparison } = await pullData(token, owner, repo, number);

  return <PullCommits owner={owner} repo={repo} comparison={comparison} />;
}
