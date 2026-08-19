import { CommitsView } from "@/components/repo/commits";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string; repo: string }>;
  searchParams: Promise<{ ref?: string; from?: string }>;
}) {
  const { owner, repo } = await params;
  const { token } = await guardRepo(owner, repo);
  const { ref, from } = await searchParams;
  return <CommitsView token={token} owner={owner} repo={repo} refName={ref} from={from} />;
}
