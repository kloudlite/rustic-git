import { getSession } from "@/lib/session";

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
  // Signed out, `/{owner}` is a team's public profile and draws its own frame — the
  // marketing header plus its own container. Wrapping it here would nest one page
  // container inside another. Every other page in the group guards itself and
  // redirects, so there is nothing to protect by redirecting from the layout.
  if (!session?.user.username) return <>{children}</>;
  // The page frame, so every page in the namespace shares one width and one set
  // of margins rather than each restating them. The chrome above it belongs to
  // the shell layout, which stays mounted across all of this.
  return <main className="mx-auto max-w-page px-6 pt-8 pb-16">{children}</main>;
}
