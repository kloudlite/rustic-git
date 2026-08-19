import { RepoPage } from "@/components/repo/repo-page";
import { CommitsView } from "@/components/repo/commits";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string; repo: string }>;
  searchParams: Promise<{ ref?: string; from?: string }>;
}) {
  const { session, owner, repo, meta, token } = await guardRepo(params);
  const { ref, from } = await searchParams;
  return <RepoPage session={session} repo={repo} visibility={meta.public ? "public" : "private"} active="Code"><CommitsView token={token} owner={owner} repo={repo} refName={ref} from={from} /></RepoPage>;
}
