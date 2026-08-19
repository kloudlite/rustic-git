import { AppShell } from "@/components/app/app-shell";
import { guardRepo } from "./guard";

/**
 * Every repo route, in one place.
 *
 * The shell — breadcrumb, repo tabs, scroll region — belongs here rather than in
 * each page. Eleven pages each rebuilding it is how they drift: one forgets a
 * prop, another passes a different one, and the chrome changes as you navigate
 * through what is supposed to be the same repo.
 *
 * It also means the shell is not re-rendered on navigation between repo pages —
 * React keeps a layout mounted while the page beneath it changes — so the tab row
 * does not flash and the scroll position of the chrome is preserved.
 *
 * `guardRepo` is cached per request, so resolving the repo here costs the page
 * beneath nothing.
 */
export default async function RepoLayout({
  params,
  children,
}: {
  params: Promise<{ owner: string; repo: string }>;
  children: React.ReactNode;
}) {
  const { owner, repo } = await params;
  const { session, meta } = await guardRepo(owner, repo);

  return (
    <AppShell
      session={session}
      context={{ kind: "repo", name: repo, visibility: meta.public ? "public" : "private" }}
    >
      <main className="mx-auto max-w-page px-6 pt-6 pb-16">{children}</main>
    </AppShell>
  );
}
