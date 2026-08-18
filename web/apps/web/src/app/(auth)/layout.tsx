import Link from "next/link";
import { Logo } from "@/components/brand/logo";
import { ThemeToggle } from "@/components/theme-toggle";

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

      <main className="flex flex-1 items-start justify-center px-6 pb-20 pt-[6vh] sm:items-center sm:pt-0">
        <div className="w-full max-w-[380px]">{children}</div>
      </main>

      <footer className="flex flex-wrap items-center justify-center gap-x-6 gap-y-2 px-6 py-6 text-[12.5px] text-muted-foreground">
        <Link href="/docs" className="transition-colors hover:text-foreground">Docs</Link>
        <Link href="/privacy" className="transition-colors hover:text-foreground">Privacy</Link>
        <Link href="/terms" className="transition-colors hover:text-foreground">Terms</Link>
      </footer>
    </div>
  );
}
