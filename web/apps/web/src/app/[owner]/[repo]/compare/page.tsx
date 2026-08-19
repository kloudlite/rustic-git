import { RepoPage } from "@/components/repo/repo-page";
import { CompareView } from "@/components/repo/compare";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { session, owner } = await guardRepo(params);
  return <RepoPage session={session} active="Pull requests"><CompareView owner={owner} /></RepoPage>;
}
