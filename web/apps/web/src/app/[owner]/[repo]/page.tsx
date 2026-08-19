import { RepoPage } from "@/components/repo/repo-page";
import { CodeView } from "@/components/repo/code";
import { guardRepo } from "./guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { session, owner } = await guardRepo(params);
  return <RepoPage session={session} active="Code"><CodeView owner={owner} /></RepoPage>;
}
