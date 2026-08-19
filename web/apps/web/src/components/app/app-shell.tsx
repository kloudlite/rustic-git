import Link from "next/link";
import { ChevronsUpDown } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { SidebarNav } from "@/components/app/sidebar-nav";
import { MobileNav } from "@/components/app/mobile-nav";
import { UserMenu } from "@/components/app/user-menu";
import type { Session } from "@/lib/session";

/** The signed-in frame: a fixed sidebar carrying identity, org and the five
 *  sections; the page owns everything to its right. On narrow screens the
 *  sidebar folds into a top bar with a drawer. */
export function AppShell({
  session,
  active,
  children,
}: {
  session: NonNullable<Session>;
  active?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="min-h-svh lg:grid lg:grid-cols-shell">
      <aside className="sticky top-0 hidden h-svh flex-col border-r border-sidebar-border bg-sidebar lg:flex">
        <div className="flex h-14 items-center px-4">
          <Link href="/" aria-label="kloudlite home" className="inline-flex">
            <Logo className="h-5" />
          </Link>
        </div>

        <div className="px-3">
          <button
            type="button"
            className="flex h-8 w-full items-center gap-2 border border-edge px-2.5 text-sm2 font-medium transition-colors hover:bg-sidebar-accent"
          >
            <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
            <span className="truncate">{session.user.owner}</span>
            <ChevronsUpDown className="ml-auto size-3.5 shrink-0 text-muted-foreground" />
          </button>
        </div>

        <div className="mt-6 px-3">
          <SidebarNav active={active} />
        </div>

        <div className="mt-auto flex items-center gap-2.5 border-t border-sidebar-border px-3 py-3">
          <UserMenu name={session.user.name} email={session.user.email} />
          <div className="min-w-0 flex-1">
            <div className="truncate text-sm2 font-medium">{session.user.name}</div>
            <div className="truncate text-caption text-muted-foreground">{session.user.email}</div>
          </div>
        </div>
      </aside>

      <div className="min-w-0">
        <MobileNav session={session} active={active} />
        {children}
      </div>
    </div>
  );
}
