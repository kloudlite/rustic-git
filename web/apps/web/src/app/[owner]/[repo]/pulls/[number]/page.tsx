import { notFound } from "next/navigation";
import { PullView } from "@/components/repo/pull-view";
import { guardRepo } from "@/app/[owner]/[repo]/guard";

export default async function Page({
  params,
}: {
  params: Promise<{ owner: string; repo: string; number: string }>;
}) {
  const { owner, repo, number } = await params;
  const { token } = await guardRepo(owner, repo);
  const n = Number(number);
  if (!Number.isInteger(n) || n < 1) notFound();
  return <PullView token={token} owner={owner} repo={repo} number={n} />;
}
