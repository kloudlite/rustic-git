import Link from "next/link";
import { Layers, Package, SquareCode, SquareTerminal, Zap } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { LoopVisual } from "@/components/marketing/loop-visual";
import { ThemeToggle } from "@/components/theme-toggle";
import { Button } from "@/components/ui/button";

/** Bodies are deliberately one short line each: the whole page is one screen, so
 *  anything that wraps to a third line pushes the strip past the fold. */
const CAPABILITIES = [
  { icon: SquareCode, title: "Code Repos", body: "Hosted, traceable source." },
  { icon: Package, title: "Package Registries", body: "Artifacts beside their code." },
  { icon: SquareTerminal, title: "Workspaces", body: "AI-ready, defined in the repo." },
  { icon: Layers, title: "Environments", body: "Fork and switch, keep state." },
  { icon: Zap, title: "CI Triggers", body: "A push builds and ships." },
];

export function Landing() {
  return (
    /* h-svh, not min-h-svh: this page is exactly one screen and must not scroll. */
    <div className="flex h-svh flex-col overflow-hidden">
      <header className="shrink-0">
        <div className="mx-auto flex h-14 max-w-[1120px] items-center gap-4 px-6">
          <Link href="/" aria-label="kloudlite home"><Logo className="h-5" /></Link>
          <nav className="ml-4 hidden items-center gap-5 text-[13.5px] text-muted-foreground md:flex">
            <a href="https://kloudlite.io/docs" className="transition-colors hover:text-foreground">Docs</a>
            <a href="https://kloudlite.io/pricing" className="transition-colors hover:text-foreground">Pricing</a>
          </nav>
          <div className="flex-1" />
          <Button asChild variant="ghost" className="h-8 px-3 text-[13.5px] font-semibold">
            <Link href="/login">Sign in</Link>
          </Button>
          <Button asChild className="h-8 px-4 text-[13.5px] font-semibold">
            <Link href="/signup">Get started</Link>
          </Button>
        </div>
      </header>

      <main className="mx-auto flex w-full max-w-[1120px] flex-1 flex-col justify-center px-6 py-8">
        <div className="grid items-center gap-10 lg:grid-cols-[minmax(0,1fr)_minmax(0,380px)]">
          <div>
            <p className="text-[12.5px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
              Cloud development environments
            </p>
            <h1 className="mt-5 max-w-[640px] text-[clamp(30px,4.2vw,46px)] font-bold leading-[1.08] tracking-[-0.02em]">
              Designed to reduce the development loop.
            </h1>
            <p className="mt-6 max-w-[560px] text-[15.5px] leading-relaxed text-muted-foreground">
              No setup, no builds, no deployments. Your code, its packages, the workspace you write
              it in and the environment it runs in are one system — and every session, yours or an
              agent&rsquo;s, forks its own.
            </p>

            <div className="mt-8 flex flex-wrap items-center gap-3">
              <Button asChild className="h-11 px-6 text-[14.5px] font-semibold">
                <Link href="/signup">Get started</Link>
              </Button>
              <Button
                asChild
                variant="outline"
                className="h-11 border-foreground/20 px-6 text-[14.5px] font-semibold transition-colors hover:border-foreground/30"
              >
                <a href="https://kloudlite.io/docs">Read the docs</a>
              </Button>
            </div>
          </div>

          <LoopVisual className="hidden h-auto w-full max-w-[340px] justify-self-end lg:block" />
        </div>

        {/* The five parts, named on the same screen as the promise they support. */}
        <ul className="mt-14 grid grid-cols-2 gap-x-8 gap-y-6 sm:grid-cols-3 lg:grid-cols-5 lg:gap-x-0">
          {CAPABILITIES.map(({ icon: Icon, title, body }) => (
            <li
              key={title}
              className="lg:border-l lg:border-foreground/[0.07] lg:px-6 lg:first:border-l-0 lg:first:pl-0 lg:last:pr-0"
            >
              <div className="flex items-center gap-2">
                <Icon className="size-4 shrink-0 text-primary" />
                <h2 className="text-[13.5px] font-bold tracking-[-0.005em]">{title}</h2>
              </div>
              <p className="mt-1.5 text-[13px] leading-snug text-muted-foreground">{body}</p>
            </li>
          ))}
        </ul>
      </main>

      <footer className="shrink-0">
        <div className="mx-auto flex max-w-[1120px] flex-wrap items-center gap-x-6 gap-y-1 px-6 py-4 text-[12.5px] text-muted-foreground">
          <span>© {new Date().getFullYear()} kloudlite</span>
          <a href="https://kloudlite.io/privacy" className="transition-colors hover:text-foreground">Privacy</a>
          <a href="https://kloudlite.io/terms" className="transition-colors hover:text-foreground">Terms</a>
          <a href="https://kloudlite.io/branding" className="transition-colors hover:text-foreground">Branding</a>
          <ThemeToggle className="ml-auto" />
        </div>
      </footer>
    </div>
  );
}
