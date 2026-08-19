import { PullHeader } from "@/components/repo/pull-page";
import { PullCommits } from "@/components/repo/pull-commits";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string; number: string }> }) {
  const { owner, repo } = await params;
  await guardRepo(owner, repo);
  return (
    <>
      <PullHeader owner={owner} />
      <PullCommits owner={owner} />
    </>
  );
}
