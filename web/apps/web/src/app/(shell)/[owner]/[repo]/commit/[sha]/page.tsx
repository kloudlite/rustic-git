import { DiffView } from "@/components/repo/diff";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string; sha: string }> }) {
  const { owner, repo, sha } = await params;
  const { token } = await guardRepo(owner, repo);
  return <DiffView token={token} owner={owner} repo={repo} sha={sha} />;
}
