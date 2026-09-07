import { getSession } from "@/lib/session";
import { MarketingHeader } from "@/components/marketing/marketing-header";

/**
 * Every page of an owner's namespace — repos, registries, workspaces,
 * environments, CI, settings — under one shell.
 *
 * A route GROUP, so it wraps these pages without wrapping `[owner]/[repo]`,
 * which has a shell of its own with the repo's tabs. The URL is unchanged:
 * `(org)` is a grouping, not a path segment.
 *
 * The reason it is a layout rather than a component each page renders: React
 * keeps a layout mounted while the page beneath it changes. The tab row's
 * underline slides between tabs, and a row that is torn down and rebuilt on every
 * navigation cannot animate — it can only reappear somewhere else.
 */
export default async function OrgLayout({ children }: { children: React.ReactNode }) {
  const session = await getSession();
  // Signed out, `/{owner}` is a team's public profile: the marketing header plus the
  // same container. It lives HERE rather than in the page because loading.tsx renders
  // in the layout's place — framed here, the skeleton lands in the same frame the page
  // does instead of painting full-bleed and then jumping. Every other page in the group
  // guards itself and redirects, so there is nothing to protect by redirecting here.
  if (!session?.user.username)
    return (
      <>
        <MarketingHeader />
        <main className="mx-auto max-w-page px-6 pt-8 pb-16">{children}</main>
      </>
    );
  // The page frame, so every page in the namespace shares one width and one set
  // of margins rather than each restating them. The chrome above it belongs to
  // the shell layout, which stays mounted across all of this.
  return <main className="mx-auto max-w-page px-6 pt-8 pb-16">{children}</main>;
}
