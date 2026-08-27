import Link from "next/link";
import { Logo } from "@/components/brand/logo";
import { Button } from "@/components/ui/button";

/** kloudlite.io's nav hover: a 2px brand underline that grows from the left over
 *  300ms. The link keeps its own padding-bottom so the rule has somewhere to sit. */
export const NAV_LINK = "nav-link";

/** The signed-out top bar. Shared by the landing page and by pages an anonymous
 *  visitor can reach directly (a team's public profile), which get no app shell. */
export function MarketingHeader() {
  return (
    <header className="shrink-0 border-b border-border bg-card">
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
  );
}
