"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useState } from "react";
import { Menu, X } from "lucide-react";
import type { NavSection } from "@/lib/docs";
import { cn } from "@/lib/utils";

/** The section tree. Sections are flat headings, not accordions: thirty-nine pages fit on one
 *  scroll, and a tree that hides itself is a tree people stop reading. */
export function Sidebar({ sections, onNavigate }: { sections: NavSection[]; onNavigate?: () => void }) {
  const pathname = usePathname();
  return (
    <nav className="docs-nav" aria-label="Documentation">
      {sections.map((s) => (
        <div key={s.section} className="docs-nav-section">
          <div className="docs-nav-heading">{s.section}</div>
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
        </div>
      ))}
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
