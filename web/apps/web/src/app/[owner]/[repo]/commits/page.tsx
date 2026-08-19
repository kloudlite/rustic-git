import { RepoPage } from "@/components/repo/repo-page";
import { CommitsView } from "@/components/repo/commits";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { session, owner, repo, meta } = await guardRepo(params);
  return <RepoPage session={session} repo={repo} visibility={meta.public ? "public" : "private"} active="Code"><CommitsView owner={owner} /></RepoPage>;
}
