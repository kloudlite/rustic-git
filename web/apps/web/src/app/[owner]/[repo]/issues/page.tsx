import { IssuesView } from "@/components/repo/issues";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { owner, repo } = await params;
  await guardRepo(owner, repo);
  return <IssuesView owner={owner} />;
}
