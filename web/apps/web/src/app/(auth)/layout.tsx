import Link from "next/link";
import { Icon } from "@/components/brand/logo";
import { ThemeToggle } from "@/components/theme-toggle";
import { ScrollArea } from "@/components/ui/scroll-area";

const NAV_LINK = "nav-link";

/** One centred column: mark, card, footnote. No header bar and no marketing panel —
 *  someone reaching this page has already decided; the job is to get them through it.
 *  The mark sits above the card rather than in a corner so the page has one axis. */
export default function AuthLayout({ children }: { children: React.ReactNode }) {
  return (
    <ScrollArea className="h-screen">
      <div className="flex min-h-screen flex-col bg-muted/40 dark:bg-background">
      <main className="flex flex-1 flex-col items-center justify-center px-6 py-16">
        <Link href="/" aria-label="kloudlite home" className="mb-8 inline-flex">
          <Icon className="size-9" />
        </Link>
        <div className="w-full max-w-auth">{children}</div>
      </main>

      <footer className="flex flex-wrap items-center gap-x-6 gap-y-2 px-6 py-4 text-caption text-muted-foreground">
        <a href="https://kloudlite.io/docs" className={NAV_LINK}>Docs</a>
        <a href="https://kloudlite.io/privacy" className={NAV_LINK}>Privacy</a>
        <a href="https://kloudlite.io/terms" className={NAV_LINK}>Terms</a>
        <ThemeToggle className="ml-auto" />
      </footer>
      </div>
    </ScrollArea>
  );
}
