import Link from "next/link";
import { Layers, Package, SquareCode, SquareTerminal, Zap } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { LoopVisual } from "@/components/marketing/loop-visual";
import { ThemeToggle } from "@/components/theme-toggle";
import { Button } from "@/components/ui/button";

/** kloudlite.io's nav hover: a 2px brand underline that grows from the left over
 *  300ms. The link keeps its own padding-bottom so the rule has somewhere to sit. */
const NAV_LINK = "nav-link";

/** Bodies are deliberately one short line each: the whole page is one screen, so
 *  anything that wraps to a third line pushes the strip past the fold. */
/** Column width at lg is 224px less px-6, so ~176px — about 24 characters at
 *  13px. Every body stays under that so all five sit on one line and the row
 *  bottoms align; anything longer wraps on some columns and not others. */
const CAPABILITIES = [
  { icon: SquareCode, title: "Code Repos", body: "Hosted, fully traceable." },
  { icon: Package, title: "Package Registries", body: "Artifacts beside code." },
  { icon: SquareTerminal, title: "Workspaces", body: "AI-ready, from the repo." },
  { icon: Layers, title: "Environments", body: "Forked, switched, kept." },
  { icon: Zap, title: "CI Triggers", body: "Built and shipped." },
];

export function Landing() {
  return (
    /* h-svh, not min-h-svh: this page is exactly one screen and must not scroll. */
    <div className="flex h-svh flex-col overflow-hidden">
      <header className="shrink-0">
        <div className="mx-auto flex h-14 max-w-page items-center gap-4 px-6">
          <Link href="/" aria-label="kloudlite home"><Logo className="h-5" /></Link>
          <nav className="ml-4 hidden items-center gap-5 text-sm2 text-muted-foreground md:flex">
            <a href="https://kloudlite.io/docs" className={NAV_LINK}>Docs</a>
            <a href="https://kloudlite.io/pricing" className={NAV_LINK}>Pricing</a>
          </nav>
          <div className="flex-1" />
          <div className="flex items-center gap-2">
            <Button
              asChild
              variant="outline"
              className="border-edge hover:border-edge-hover"
            >
              <Link href="/login">Sign in</Link>
            </Button>
            <Button asChild>
              <Link href="/signup">Get started</Link>
            </Button>
          </div>
        </div>
      </header>

      <main className="mx-auto flex w-full max-w-page flex-1 flex-col justify-center px-6 py-8">
        <div className="grid items-center gap-10 lg:grid-cols-hero">
          <div>
            <p className="text-caption font-semibold uppercase tracking-eyebrow text-muted-foreground">
              Environments that keep up with your agents
            </p>
            <h1 className="mt-5 max-w-prose text-display font-bold leading-display tracking-display">
              Extend your{" "}
              <span className="text-highlight">
                agentic loops
              </span>{" "}
              beyond the codebase.
            </h1>
            <p className="mt-6 max-w-prose text-lead leading-relaxed text-muted-foreground">
              No setup, no builds, no deployments. Code, packages, workspace and environment are
              one system — so any session, yours or an agent&rsquo;s, forks all four at once.
            </p>

            <div className="mt-8 flex flex-wrap items-center gap-3">
              <Button asChild className="h-11 px-5">
                <Link href="/signup">Get started</Link>
              </Button>
              <Button
                asChild
                variant="outline"
                className="h-11 border-edge px-5 transition-colors hover:border-edge-hover"
              >
                <a href="https://kloudlite.io/docs">Read the docs</a>
              </Button>
            </div>
          </div>

          <LoopVisual className="hidden h-auto w-full max-w-figure justify-self-end lg:block" />
        </div>

        {/* The five parts, named on the same screen as the promise they support. */}
        <ul className="mt-14 grid grid-cols-2 gap-x-8 gap-y-6 sm:grid-cols-3 lg:grid-cols-5 lg:gap-x-0">
          {CAPABILITIES.map(({ icon: Icon, title, body }) => (
            <li
              key={title}
              className="lg:border-l lg:border-rule lg:px-6 lg:first:border-l-0 lg:first:pl-0 lg:last:pr-0"
            >
              <div className="flex items-center gap-2">
                <Icon className="size-4 shrink-0 text-primary" />
                <h2 className="text-sm2 font-bold tracking-title">{title}</h2>
              </div>
              <p className="mt-1.5 text-sm2 leading-snug text-muted-foreground">{body}</p>
            </li>
          ))}
        </ul>
      </main>

      <footer className="shrink-0">
        <div className="mx-auto flex max-w-page flex-wrap items-center gap-x-6 gap-y-1 px-6 py-4 text-caption text-muted-foreground">
          <span>© {new Date().getFullYear()} kloudlite</span>
          <a href="https://kloudlite.io/privacy" className={NAV_LINK}>Privacy</a>
          <a href="https://kloudlite.io/terms" className={NAV_LINK}>Terms</a>
          <a href="https://kloudlite.io/branding" className={NAV_LINK}>Branding</a>
          <ThemeToggle className="ml-auto" />
        </div>
      </footer>
    </div>
  );
}
