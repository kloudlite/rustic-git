import { CompareView } from "@/components/repo/compare";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { owner, repo } = await params;
  await guardRepo(owner, repo);
  return <CompareView owner={owner} />;
}
