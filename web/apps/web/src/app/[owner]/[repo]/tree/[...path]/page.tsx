import { RepoPage } from "@/components/repo/repo-page";
import { CodeView } from "@/components/repo/code";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string; repo: string; path: string[] }>;
  searchParams: Promise<{ ref?: string }>;
}) {
  const { session, owner, repo, meta, token } = await guardRepo(params);
  const { ref } = await searchParams;
  const { path } = await params;
  return <RepoPage session={session} repo={repo} visibility={meta.public ? "public" : "private"} active="Code"><CodeView token={token} owner={owner} repo={repo} meta={meta} dir={path.join("/")} refName={ref} /></RepoPage>;
}
