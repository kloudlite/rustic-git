"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useState } from "react";
import { ChevronRight, Menu, X } from "lucide-react";
import type { NavSection } from "@/lib/docs";
import { cn } from "@/lib/utils";

/** The section tree. The first three groups stay open; every other group opens when it holds
 *  the current page and folds otherwise, so forty pages read as ten headings. */
export function Sidebar({ sections, onNavigate }: { sections: NavSection[]; onNavigate?: () => void }) {
  const pathname = usePathname();
  return (
    <nav className="docs-nav" aria-label="Documentation">
      {sections.map((s, i) => {
        const holds = s.items.some((it) => it.href === pathname);
        return (
          <details key={s.section} className="docs-nav-group" open={i < 2 || holds}>
            <summary className="docs-nav-heading">
              <ChevronRight className="docs-nav-chev" aria-hidden />
              {s.section}
            </summary>
            <ul>
              {s.items.map((it) => {
                const active = pathname === it.href;
                return (
                  <li key={`${s.section}/${it.slug}`}>
                    <Link href={it.href} onClick={onNavigate} className={cn("docs-nav-link", active && "is-active")} aria-current={active ? "page" : undefined}>
                      {it.title}
                    </Link>
                  </li>
                );
              })}
            </ul>
          </details>
        );
      })}
    </nav>
  );
}

/** Below 1280 px the tree lives behind this button. */
export function MobileNav({ sections }: { sections: NavSection[] }) {
  const [open, setOpen] = useState(false);
  useEffect(() => {
    document.body.style.overflow = open ? "hidden" : "";
    return () => {
      document.body.style.overflow = "";
    };
  }, [open]);
  return (
    <>
      <button type="button" className="docs-menu-btn" aria-label={open ? "Close navigation" : "Open navigation"} aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        {open ? <X className="size-4" /> : <Menu className="size-4" />}
      </button>
      {open && (
        <div className="docs-drawer" role="dialog" aria-label="Documentation navigation">
          <Sidebar sections={sections} onNavigate={() => setOpen(false)} />
        </div>
      )}
    </>
  );
}
