import { RepoPage } from "@/components/repo/repo-page";
import { CompareView } from "@/components/repo/compare";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { session, owner, repo, meta } = await guardRepo(params);
  return <RepoPage session={session} repo={repo} visibility={meta.public ? "public" : "private"} active="Pull requests"><CompareView owner={owner} /></RepoPage>;
}
