import { RepoPage } from "@/components/repo/repo-page";
import { PullHeader } from "@/components/repo/pull-page";
import { PullCommits } from "@/components/repo/pull-commits";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string; number: string }> }) {
  const { session, owner } = await guardRepo(params);
  return (
    <RepoPage session={session} active="Pull requests">
      <PullHeader owner={owner} tab="commits" />
      <PullCommits owner={owner} />
    </RepoPage>
  );
}
