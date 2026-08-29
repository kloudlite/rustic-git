import type { Metadata } from "next";
import { SetRepoMeta } from "@/components/app/shell-context";
import { guardRepo } from "./guard";

/**
 * Every repo route, in one place.
 *
 * The chrome is NOT here: one shell wraps every signed-in page and stays mounted
 * across all of them, which is what lets the tab row slide rather than reappear.
 * What this layout owns is the page frame, and telling that shell the one thing
 * it cannot read off the URL — whether this repo is public.
 *
 * `guardRepo` is cached per request, so resolving the repo here costs the page
 * beneath nothing, and refusing here means no page under it has to check.
 */
/** The tab is named after the repo; each page's own `metadata` (Settings, Pull requests)
 *  still wins where it is set. */
export async function generateMetadata({ params }: { params: Promise<{ owner: string; repo: string }> }): Promise<Metadata> {
  const { owner, repo } = await params;
  return { title: `${owner}/${repo}` };
}

export default async function RepoLayout({
  params,
  children,
}: {
  params: Promise<{ owner: string; repo: string }>;
  children: React.ReactNode;
}) {
  const { owner, repo } = await params;
  const { meta } = await guardRepo(owner, repo);

  return (
    <>
      <SetRepoMeta visibility={meta.public ? "public" : "private"} />
      <main className="mx-auto max-w-page px-6 pt-6 pb-16">{children}</main>
    </>
  );
}
