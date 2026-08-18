import Link from "next/link";
import { Layers, Package, SquareCode, SquareTerminal, Zap } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { LoopVisual } from "@/components/marketing/loop-visual";
import { ThemeToggle } from "@/components/theme-toggle";
import { Button } from "@/components/ui/button";

/** kloudlite.io's nav hover: a 2px brand underline that grows from the left over
 *  300ms. The link keeps its own padding-bottom so the rule has somewhere to sit. */
const NAV_LINK =
  "relative py-1 transition-colors hover:text-foreground " +
  "after:absolute after:bottom-0 after:left-0 after:h-[2px] after:w-0 after:bg-primary " +
  "after:transition-all after:duration-300 hover:after:w-full";

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
        <div className="mx-auto flex h-14 max-w-[1120px] items-center gap-4 px-6">
          <Link href="/" aria-label="kloudlite home"><Logo className="h-5" /></Link>
          <nav className="ml-4 hidden items-center gap-5 text-[13.5px] text-muted-foreground md:flex">
            <a href="https://kloudlite.io/docs" className={NAV_LINK}>Docs</a>
            <a href="https://kloudlite.io/pricing" className={NAV_LINK}>Pricing</a>
          </nav>
          <div className="flex-1" />
          <Button
            asChild
            variant="outline"
            className="h-8 border-foreground/[0.12] px-4 text-[13.5px] hover:border-foreground/20"
          >
            <Link href="/login">Sign in</Link>
          </Button>
          <Button asChild className="h-8 px-4 text-[13.5px]">
            <Link href="/signup">Get started</Link>
          </Button>
        </div>
      </header>

      <main className="mx-auto flex w-full max-w-[1120px] flex-1 flex-col justify-center px-6 py-8">
        <div className="grid items-center gap-10 lg:grid-cols-[minmax(0,1fr)_minmax(0,380px)]">
          <div>
            <p className="text-[12.5px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
              Environments that keep up with your agents
            </p>
            <h1 className="mt-5 max-w-[640px] text-[clamp(30px,4.2vw,46px)] font-bold leading-[1.08] tracking-[-0.02em]">
              Designed to reduce your{" "}
              <span className="underline decoration-primary decoration-[3px] underline-offset-[10px]">
                agentic loops.
              </span>
            </h1>
            <p className="mt-6 max-w-[560px] text-[15.5px] leading-relaxed text-muted-foreground">
              No setup, no builds, no deployments. Code, packages, workspace and environment are
              one system — so any session, yours or an agent&rsquo;s, forks all four at once.
            </p>

            <div className="mt-8 flex flex-wrap items-center gap-3">
              <Button asChild className="h-11 px-6 text-[14.5px]">
                <Link href="/signup">Get started</Link>
              </Button>
              <Button
                asChild
                variant="outline"
                className="h-11 border-foreground/[0.12] px-6 text-[14.5px] transition-colors hover:border-foreground/20"
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
          <a href="https://kloudlite.io/privacy" className={NAV_LINK}>Privacy</a>
          <a href="https://kloudlite.io/terms" className={NAV_LINK}>Terms</a>
          <a href="https://kloudlite.io/branding" className={NAV_LINK}>Branding</a>
          <ThemeToggle className="ml-auto" />
        </div>
      </footer>
    </div>
  );
}
