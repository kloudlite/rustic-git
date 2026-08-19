import { RepoPage } from "@/components/repo/repo-page";
import { FileView } from "@/components/repo/file-view";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string; path: string[] }> }) {
  const { session, owner } = await guardRepo(params);
  return <RepoPage session={session} active="Code"><FileView owner={owner} /></RepoPage>;
}
