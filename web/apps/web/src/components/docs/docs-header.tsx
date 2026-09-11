import Link from "next/link";
import { Logo } from "@/components/brand/logo";
import { Button } from "@/components/ui/button";
import { ThemeToggle } from "@/components/theme-toggle";
import { Search } from "@/components/docs/search";
import { MobileNav } from "@/components/docs/sidebar";
import type { NavSection } from "@/lib/docs";

export function DocsHeader({
  index,
  sections,
}: {
  index: { title: string; section: string; href: string; headings: { text: string; id: string }[] }[];
  sections: NavSection[];
}) {
  return (
    <header className="docs-header">
      <div className="docs-header-in">
        <MobileNav sections={sections} />
        <Link href="/" aria-label="kloudlite home" className="flex items-center gap-2.5">
          <Logo className="h-5" />
        </Link>
        <Link href="/docs" className="docs-wordmark">Docs</Link>
        <div className="flex-1" />
        <Search index={index} />
        <nav className="docs-header-links">
          <a href="https://kloudlite.io" target="_blank" rel="noreferrer">kloudlite.io</a>
        </nav>
        <ThemeToggle />
        <Button asChild size="sm">
          <Link href="/login">Open app</Link>
        </Button>
      </div>
    </header>
  );
}
