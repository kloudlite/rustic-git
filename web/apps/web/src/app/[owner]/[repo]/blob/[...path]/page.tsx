import { RepoPage } from "@/components/repo/repo-page";
import { FileView } from "@/components/repo/file-view";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string; repo: string; path: string[] }>;
  searchParams: Promise<{ ref?: string }>;
}) {
  const { session, owner, repo, meta, token } = await guardRepo(params);
  const { path } = await params;
  const { ref } = await searchParams;
  return (
    <RepoPage session={session} repo={repo} visibility={meta.public ? "public" : "private"} active="Code">
      <FileView token={token} owner={owner} repo={repo} meta={meta} path={path.join("/")} refName={ref} />
    </RepoPage>
  );
}
