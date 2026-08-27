import { PullConversation } from "@/components/repo/pull-conversation";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";
import { pullData } from "./pull-data";

export default async function Page({
  params,
}: {
  params: Promise<{ owner: string; repo: string; number: string }>;
}) {
  const { owner, repo, number } = await params;
  const { token } = await guardRepo(owner, repo);
  const { pull } = await pullData(token, owner, repo, Number(number));

  return <PullConversation owner={owner} repo={repo} pull={pull} />;
}
