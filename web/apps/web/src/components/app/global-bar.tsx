import Link from "next/link";
import { ChevronsUpDown, Search } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { ThemeToggle } from "@/components/theme-toggle";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { cn } from "@/lib/utils";
import { signOutAction } from "@/app/(auth)/actions";
import type { Session } from "@/lib/session";

const SECTIONS = [
  { href: "/kloudlite", label: "Code Repos" },
  { href: "/kloudlite/registries", label: "Package Registries" },
  { href: "/kloudlite/workspaces", label: "Workspaces" },
  { href: "/kloudlite/environments", label: "Environments" },
  { href: "/kloudlite/ci", label: "CI Triggers" },
];

/** Tier 1. Which org, which resource type — and nothing else. Every other location
 *  fact is stated exactly once, further down the page. */
export function GlobalBar({ session, active }: { session: NonNullable<Session>; active?: string }) {
  const initials = session.user.name.split(" ").map((p) => p[0]).slice(0, 2).join("");

  return (
    <header className="sticky top-0 z-40 border-b border-border bg-background">
      <div className="flex h-14 items-center gap-3 px-4 md:px-6">
        <Link href="/" aria-label="kloudlite home" className="shrink-0">
          <Logo className="h-5" />
        </Link>

        <button
          type="button"
          className="ml-1 hidden items-center gap-2 border border-border px-2.5 py-1.5 text-sm2 font-semibold hover:bg-muted sm:flex"
        >
          <span className="size-4 bg-primary" aria-hidden />
          {session.user.owner}
          <ChevronsUpDown className="size-3.5 text-muted-foreground" />
        </button>

        <nav className="ml-2 hidden items-center gap-0.5 lg:flex">
          {SECTIONS.map((s) => (
            <Link
              key={s.href}
              href={s.href}
              className={cn(
                "px-3 py-1.5 text-sm2 transition-colors",
                active === s.label
                  ? "bg-muted font-semibold text-foreground"
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
          className="hidden h-9 w-52 items-center gap-2 border border-border px-2.5 text-left text-sm2 text-muted-foreground hover:bg-muted md:flex"
        >
          <Search className="size-3.5" />
          Search
          <kbd className="ml-auto border border-border px-1 py-px font-mono text-micro">⌘K</kbd>
        </button>

        <ThemeToggle className="hidden sm:inline-flex" />

        <Avatar className="size-8 rounded-none">
          <AvatarFallback className="rounded-none bg-primary text-micro font-semibold text-primary-foreground">
            {initials}
          </AvatarFallback>
        </Avatar>
        <form action={signOutAction}>
          <button
            type="submit"
            className="text-sm2 text-muted-foreground transition-colors hover:text-foreground"
          >
            Sign out
          </button>
        </form>
      </div>

      {/* Sections wrap to their own row below lg rather than collapsing into a menu:
          they are the primary navigation and should not need a tap to discover. */}
      <nav className="flex items-center gap-0.5 overflow-x-auto px-4 pb-2 lg:hidden md:px-6">
        {SECTIONS.map((s) => (
          <Link
            key={s.href}
            href={s.href}
            className={cn(
              "whitespace-nowrap px-3 py-1.5 text-sm2 transition-colors",
              active === s.label
                ? "bg-muted font-semibold text-foreground"
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
