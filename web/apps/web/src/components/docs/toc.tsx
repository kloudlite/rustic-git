"use client";

import { useEffect, useState } from "react";
import type { Heading } from "@/lib/docs";
import { cn } from "@/lib/utils";

/** The page's own outline, with the section under the reader marked. One observer over the
 *  headings; the topmost visible one wins, and past the last heading the last one stays lit. */
export function Toc({ headings }: { headings: Heading[] }) {
  const [active, setActive] = useState<string | null>(headings[0]?.id ?? null);
  useEffect(() => {
    if (headings.length === 0) return;
    const els = headings.map((h) => document.getElementById(h.id)).filter((e): e is HTMLElement => !!e);
    const visible = new Set<string>();
    const obs = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (e.isIntersecting) visible.add(e.target.id);
          else visible.delete(e.target.id);
        }
        const first = headings.find((h) => visible.has(h.id));
        if (first) setActive(first.id);
      },
      { rootMargin: "-72px 0px -70% 0px", threshold: 0 },
    );
    els.forEach((e) => obs.observe(e));
    return () => obs.disconnect();
  }, [headings]);
  if (headings.length === 0) return null;
  return (
    <nav className="docs-toc" aria-label="On this page">
      <div className="docs-toc-heading">On this page</div>
      <ul>
        {headings.map((h) => (
          <li key={h.id} className={cn(h.depth === 3 && "is-sub")}>
            <a href={`#${h.id}`} className={cn("docs-toc-link", active === h.id && "is-active")}>
              {h.text}
            </a>
          </li>
        ))}
      </ul>
    </nav>
  );
}
