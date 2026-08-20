import { PullsView } from "@/components/repo/pulls";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { owner, repo } = await params;
  const { token } = await guardRepo(owner, repo);
  return <PullsView token={token} owner={owner} repo={repo} />;
}
