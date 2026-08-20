import type { Metadata } from "next";
import { listProtection } from "@/lib/api";
import { RepoSettings } from "@/components/repo/repo-settings";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";

export const metadata: Metadata = { title: "Repository settings" };

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { owner, repo } = await params;
  const { meta, token } = await guardRepo(owner, repo);
  // A rule list that cannot be read is shown as empty rather than as an error:
  // the rest of the page still works, and the fleet is still enforcing whatever
  // it has.
  const rules = await listProtection(token, owner, repo);
  return <RepoSettings meta={meta} rules={rules.ok ? rules.value : []} />;
}
