import { getSession } from "@/lib/session";
import { AppShell } from "@/components/app/app-shell";
import { AutoRefresh } from "@/components/app/auto-refresh";

/**
 * One shell for every signed-in page — home, the namespace, and every repo.
 *
 * The whole point is that it is a LAYOUT and there is exactly one: React keeps a
 * layout mounted while the page beneath it changes, so the tab row's underline
 * slides between tabs instead of being rebuilt somewhere else. Six pages used to
 * mount their own, and crossing between them was a cut rather than a motion.
 *
 * Signed out, this is the landing page, which has no chrome and needs none — so
 * the shell is skipped rather than rendered empty. Every page under here that
 * requires a session redirects on its own; this decides only what wraps them.
 *
 * `AutoRefresh` lives here for the same reason: one timer, mounted once, refreshing whichever page
 * is currently beneath the shell. Signed-out pages show nothing that changes on its own and get
 * none.
 */
export default async function ShellLayout({ children }: { children: React.ReactNode }) {
  const session = await getSession();
  if (!session?.user.username) return <>{children}</>;
  return (
    <AppShell session={session}>
      <AutoRefresh />
      {children}
    </AppShell>
  );
}
