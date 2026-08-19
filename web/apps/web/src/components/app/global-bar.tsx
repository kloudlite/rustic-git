import Link from "next/link";
import { ChevronsUpDown, Search } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { UserMenu } from "@/components/app/user-menu";
import { cn } from "@/lib/utils";
import type { Session } from "@/lib/session";

const SECTIONS = [
  { href: "/kloudlite", label: "Code Repos" },
  { href: "/kloudlite/registries", label: "Package Registries" },
  { href: "/kloudlite/workspaces", label: "Workspaces" },
  { href: "/kloudlite/environments", label: "Environments" },
  { href: "/kloudlite/ci", label: "CI Triggers" },
];

/** Tier 1. Which org, which resource type — and nothing else. Every other location
 *  fact is stated exactly once, further down the page.
 *
 *  Every control on the row is 32px tall, on one baseline. */
export function GlobalBar({ session, active }: { session: NonNullable<Session>; active?: string }) {
  return (
    <header className="sticky top-0 z-40 border-b border-border bg-background">
      <div className="flex h-14 items-center gap-2 px-4 md:px-6">
        <Link href="/" aria-label="kloudlite home" className="mr-2 shrink-0">
          <Logo className="h-5" />
        </Link>

        <button
          type="button"
          className="hidden h-8 items-center gap-2 border border-edge px-2.5 text-sm2 font-medium transition-colors hover:bg-muted sm:flex"
        >
          <span className="size-3.5 bg-primary" aria-hidden />
          {session.user.owner}
          <ChevronsUpDown className="size-3.5 text-muted-foreground" />
        </button>

        <nav className="ml-2 hidden items-center gap-0.5 lg:flex">
          {SECTIONS.map((s) => (
            <Link
              key={s.href}
              href={s.href}
              className={cn(
                "flex h-8 items-center whitespace-nowrap px-2.5 text-sm2 transition-colors",
                active === s.label
                  ? "bg-muted font-medium text-foreground"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              {s.label}
            </Link>
          ))}
        </nav>

        <div className="flex-1" />

        <button
          type="button"
          className="hidden h-8 w-48 items-center gap-2 border border-edge px-2.5 text-left text-sm2 text-muted-foreground transition-colors hover:bg-muted md:flex"
        >
          <Search className="size-3.5" />
          Search
          <kbd className="ml-auto border border-border px-1 font-mono text-micro leading-4">⌘K</kbd>
        </button>

        <div className="ml-1">
          <UserMenu name={session.user.name} email={session.user.email} />
        </div>
      </div>

      {/* Sections wrap to their own row below lg rather than collapsing into a menu:
          they are the primary navigation and should not need a tap to discover. */}
      <nav className="flex items-center gap-0.5 overflow-x-auto px-4 pb-2 lg:hidden md:px-6">
        {SECTIONS.map((s) => (
          <Link
            key={s.href}
            href={s.href}
            className={cn(
              "flex h-8 items-center whitespace-nowrap px-2.5 text-sm2 transition-colors",
              active === s.label
                ? "bg-muted font-medium text-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {s.label}
          </Link>
        ))}
      </nav>
    </header>
  );
}
