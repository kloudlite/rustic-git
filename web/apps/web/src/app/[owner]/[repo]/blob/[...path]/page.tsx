import { RepoPage } from "@/components/repo/repo-page";
import { FileView } from "@/components/repo/file-view";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string; path: string[] }> }) {
  const { session, owner, repo, meta, token } = await guardRepo(params);
  const { path } = await params;
  return (
    <RepoPage session={session} repo={repo} visibility={meta.public ? "public" : "private"} active="Code">
      <FileView token={token} owner={owner} repo={repo} meta={meta} path={path.join("/")} />
    </RepoPage>
  );
}
