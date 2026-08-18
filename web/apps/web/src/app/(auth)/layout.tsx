import Link from "next/link";
import { Logo } from "@/components/brand/logo";
import { ThemeToggle } from "@/components/theme-toggle";

/** One centred column. No marketing panel — someone reaching this page has already
 *  decided; the job is to get them through it. */
export default function AuthLayout({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex min-h-svh flex-col">
      <header className="flex items-center justify-between px-6 py-5">
        <Link href="/" aria-label="kloudlite home">
          <Logo />
        </Link>
        <ThemeToggle />
      </header>

      <main className="flex flex-1 items-center justify-center px-6 pb-16">
        <div className="w-full max-w-[360px]">{children}</div>
      </main>

      <footer className="flex flex-wrap items-center justify-center gap-x-5 gap-y-2 px-6 py-5 text-[13px] text-muted-foreground">
        <Link href="/docs" className="hover:text-foreground">Docs</Link>
        <Link href="/privacy" className="hover:text-foreground">Privacy</Link>
        <Link href="/terms" className="hover:text-foreground">Terms</Link>
      </footer>
    </div>
  );
}
