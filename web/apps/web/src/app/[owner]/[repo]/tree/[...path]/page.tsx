import { RepoPage } from "@/components/repo/repo-page";
import { CodeView } from "@/components/repo/code";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string; path: string[] }> }) {
  const { session, owner } = await guardRepo(params);
  const { path } = await params;
  return <RepoPage session={session} active="Code"><CodeView owner={owner} dir={path.join("/")} /></RepoPage>;
}
