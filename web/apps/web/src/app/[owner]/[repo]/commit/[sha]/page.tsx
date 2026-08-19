import { RepoPage } from "@/components/repo/repo-page";
import { DiffView } from "@/components/repo/diff";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string; sha: string }> }) {
  const { session, owner } = await guardRepo(params);
  return <RepoPage session={session} active="Code"><DiffView owner={owner} /></RepoPage>;
}
