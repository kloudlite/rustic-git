import { RepoPage } from "@/components/repo/repo-page";
import { CodeView } from "@/components/repo/code";
import { guardRepo } from "./guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { session, owner, repo, meta, token } = await guardRepo(params);
  return <RepoPage session={session} repo={repo} visibility={meta.public ? "public" : "private"} active="Code"><CodeView token={token} owner={owner} repo={repo} meta={meta} /></RepoPage>;
}
