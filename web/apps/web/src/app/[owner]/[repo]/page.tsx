import { RepoPage } from "@/components/repo/repo-page";
import { CodeView } from "@/components/repo/code";
import { guardRepo } from "./guard";

export default async function Page({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string; repo: string }>;
  searchParams: Promise<{ ref?: string }>;
}) {
  const { session, owner, repo, meta, token } = await guardRepo(params);
  const { ref } = await searchParams;
  return <RepoPage session={session} repo={repo} visibility={meta.public ? "public" : "private"} active="Code"><CodeView token={token} owner={owner} repo={repo} meta={meta} refName={ref} /></RepoPage>;
}
