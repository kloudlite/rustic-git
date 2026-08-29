import Link from "next/link";
import { Container, Layers, SquareCode, SquareTerminal } from "lucide-react";
import { MarketingHeader, NAV_LINK } from "@/components/marketing/marketing-header";
import { EnvironmentPanel } from "@/components/marketing/environment-panel";
import { ThemeToggle } from "@/components/theme-toggle";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";

/** Bodies are deliberately one short line each: the whole page is one screen, so
 *  anything that wraps to a third line pushes the strip past the fold. */
/** Column width at lg is 224px less px-6, so ~176px — about 24 characters at
 *  13px. Every body stays under that so all four sit on one line and the row
 *  bottoms align; anything longer wraps on some columns and not others. */
const CAPABILITIES = [
  { icon: SquareCode, title: "Code Repos", body: "Hosted, fully traceable." },
  { icon: SquareTerminal, title: "Workspaces", body: "AI-ready, from the repo." },
  { icon: Layers, title: "Environments", body: "Cloned, switched, kept." },
  { icon: Container, title: "Container Images", body: "Built here, run here." },
];

export function Landing() {
  return (
    /* Sized to one screen. min-h rather than h + overflow-hidden: at normal heights
       there is nothing to scroll, and on a short laptop viewport the strip becomes
       reachable instead of silently clipped. */
    <ScrollArea className="h-screen">
      <div className="flex min-h-screen flex-col">
      <MarketingHeader />

      <main className="mx-auto flex w-full max-w-page flex-1 flex-col justify-center px-6 py-8">
        {/* The figure column is sized here, not by a token: the environment
            panel is a readable terminal-like card, not an icon, and it needs
            ~520px before its type wraps. */}
        <div className="grid items-center gap-10 lg:grid-cols-[minmax(0,1fr)_minmax(0,520px)]">
          <div>
            <p className="text-caption font-semibold uppercase tracking-eyebrow text-muted-foreground">
              Environments that keep up with your agents
            </p>
            <h1 className="mt-4 max-w-headline text-display font-semibold leading-display tracking-display">
              Extend your{" "}
              <span className="text-highlight">
                agentic loops
              </span>{" "}
              beyond the codebase.
            </h1>
            <p className="mt-5 max-w-prose text-lead leading-relaxed text-muted-foreground">
              No setup, no builds, no deployments. Code, packages, workspace and environment are
              one system — so any session, yours or an agent&rsquo;s, clones all four at once.
            </p>

            <div className="mt-7 flex flex-wrap items-center gap-3">
              <Button asChild size="lg">
                <Link href="/signup">Get started</Link>
              </Button>
              <Button
                asChild
                variant="outline"
                size="lg"
                className="border-edge transition-colors hover:border-edge-hover"
              >
                <a href="https://kloudlite.io/docs">Read the docs</a>
              </Button>
            </div>
          </div>

          <EnvironmentPanel className="mx-auto" />
        </div>

        {/* The four parts, named on the same screen as the promise they support. */}
        <ul className="mt-14 grid grid-cols-2 gap-x-8 gap-y-6 sm:grid-cols-2 lg:grid-cols-4 lg:gap-x-0">
          {CAPABILITIES.map(({ icon: Icon, title, body }) => (
            <li
              key={title}
              className="lg:border-l lg:border-rule lg:px-6 lg:first:border-l-0 lg:first:pl-0 lg:last:pr-0"
            >
              <div className="flex items-center gap-2">
                <Icon className="size-4 shrink-0 text-primary" />
                <h2 className="text-sm2 font-semibold">{title}</h2>
              </div>
              <p className="mt-1 text-caption leading-snug text-muted-foreground">{body}</p>
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
    </ScrollArea>
  );
}
