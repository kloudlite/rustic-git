import Link from "next/link";
import { Logo } from "@/components/brand/logo";
import { ThemeToggle } from "@/components/theme-toggle";

const NAV_LINK = "nav-link";

/** One centred column. No marketing panel — someone reaching this page has already
 *  decided; the job is to get them through it. The column is 380px so the longest
 *  provider label ("Sign in with Microsoft") never crowds its own button. */
export default function AuthLayout({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex min-h-svh flex-col">
      <header className="flex items-center justify-between px-6 py-5">
        <Link href="/" aria-label="kloudlite home" className="inline-flex">
          <Logo className="h-5" />
        </Link>
        <ThemeToggle />
      </header>

      <main className="flex flex-1 items-start justify-center px-6 pb-20 pt-12 sm:items-center sm:pt-0">
        <div className="w-full max-w-auth">{children}</div>
      </main>

      <footer className="flex flex-wrap items-center justify-center gap-x-6 gap-y-2 px-6 py-6 text-caption text-muted-foreground">
        <Link href="/docs" className={NAV_LINK}>Docs</Link>
        <Link href="/privacy" className={NAV_LINK}>Privacy</Link>
        <Link href="/terms" className={NAV_LINK}>Terms</Link>
      </footer>
    </div>
  );
}
