import { RepoPage } from "@/components/repo/repo-page";
import { IssuesView } from "@/components/repo/issues";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { session, owner } = await guardRepo(params);
  return <RepoPage session={session} active="Issues"><IssuesView owner={owner} /></RepoPage>;
}
