import { CodeView } from "@/components/repo/code";
import { guardRepo } from "./guard";

export default async function Page({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string; repo: string }>;
  searchParams: Promise<{ ref?: string }>;
}) {
  const { owner, repo } = await params;
  const { meta, token } = await guardRepo(owner, repo);
  const { ref } = await searchParams;
  return <CodeView token={token} owner={owner} repo={repo} meta={meta} refName={ref} />;
}
