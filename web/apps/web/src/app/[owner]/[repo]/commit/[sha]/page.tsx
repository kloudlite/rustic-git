import { RepoPage } from "@/components/repo/repo-page";
import { DiffView } from "@/components/repo/diff";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string; sha: string }> }) {
  const { session, owner, repo, meta, token } = await guardRepo(params);
  const { sha } = await params;
  return <RepoPage session={session} repo={repo} visibility={meta.public ? "public" : "private"} active="Code"><DiffView token={token} owner={owner} repo={repo} sha={sha} /></RepoPage>;
}
