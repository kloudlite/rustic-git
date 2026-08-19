import Link from "next/link";
import { ChevronsUpDown, Search } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { SECTIONS } from "@/components/app/sections";
import { UserMenu } from "@/components/app/user-menu";
import { cn } from "@/lib/utils";
import type { Session } from "@/lib/session";

/** The signed-in frame. Two slim rows and nothing on the side:
 *    row 1 — who and where: logo, org, search, identity
 *    row 2 — the five sections as underline tabs
 *  Content runs full width beneath. Height cost is 96px total; a sidebar costs
 *  240px of width on every page for the same five links. */
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
    <div className="min-h-svh">
      <header className="sticky top-0 z-40 border-b border-border bg-background">
        <div className="mx-auto flex h-14 max-w-page items-center gap-3 px-6">
          <Link href="/" aria-label="kloudlite home" className="inline-flex">
            <Logo className="h-5" />
          </Link>
          <span className="text-muted-foreground/40" aria-hidden>/</span>
          <button
            type="button"
            className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted"
          >
            <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
            {session.user.owner}
            <ChevronsUpDown className="size-3.5 text-muted-foreground" />
          </button>

          <div className="flex-1" />

          <button
            type="button"
            className="hidden h-8 w-64 items-center gap-2 border border-edge px-2.5 text-left text-sm2 text-muted-foreground transition-colors hover:bg-muted md:flex"
          >
            <Search className="size-3.5" />
            Search
            <kbd className="ml-auto border border-border px-1 font-mono text-micro leading-4">⌘K</kbd>
          </button>
          <UserMenu name={session.user.name} email={session.user.email} />
        </div>

        <nav className="mx-auto -mb-px flex max-w-page items-stretch gap-1 px-6" aria-label="Sections">
          {SECTIONS.map(({ href, label, icon: Icon }) => {
            const isActive = active === label;
            return (
              <Link
                key={href}
                href={href}
                aria-current={isActive ? "page" : undefined}
                className={cn(
                  "flex h-10 items-center gap-2 whitespace-nowrap border-b-2 px-3 text-sm2 transition-colors",
                  isActive
                    ? "border-primary font-medium text-foreground"
                    : "border-transparent text-muted-foreground hover:border-border hover:text-foreground",
                )}
              >
                <Icon className="size-4" />
                {label}
              </Link>
            );
          })}
        </nav>
      </header>

      {children}
    </div>
  );
}
