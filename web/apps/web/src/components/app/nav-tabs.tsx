"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ArrowLeft } from "lucide-react";
import { cn } from "@/lib/utils";

export type NavTab = {
  href: string;
  label: string;
  /** Already-rendered icon element: components cannot cross into a client component. */
  icon?: React.ReactNode;
  count?: number;
  /** Pushed to the far end of the row (Settings). */
  end?: boolean;
};

const useIsoLayoutEffect = typeof window === "undefined" ? useEffect : useLayoutEffect;

/** Underline tabs, used for every level of navigation so they all behave alike.
 *
 *  Two things make them feel finished. The label reserves the width of its bold
 *  self, so activating one changes glyphs and never the box. And there is one
 *  underline, not one per tab: it measures the active label and slides to it, so
 *  moving between tabs is a motion rather than a cut. Before the first measurement
 *  (SSR, first paint) each tab draws its own static underline, so nothing flashes. */
export function NavTabs({
  tabs,
  back,
  className,
  "aria-label": ariaLabel,
}: {
  tabs: NavTab[];
  /** The list this row's subject came from. Drawn as a labelled arrow at the start
   *  of the row — same chip as a tab, so it sits on the same baseline. */
  back?: { href: string; label: string };
  className?: string;
  "aria-label"?: string;
}) {
  // Which tab is current is a fact about the URL, so it is read from the URL.
  // Passing it in meant every page that rendered this row had to name its own tab,
  // and a page that named the wrong one — or none — quietly highlighted nothing.
  const pathname = usePathname();
  const active = useMemo(() => {
    const matches = tabs
      .filter((t) => pathname === t.href || (!t.end && pathname.startsWith(`${t.href}/`)))
      // The longest matching href wins, so `/o/r/settings` picks Settings rather
      // than the `/o/r` tab that is also a prefix of it.
      .sort((a, b) => b.href.length - a.href.length);
    return matches[0]?.label;
  }, [pathname, tabs]);

  const nav = useRef<HTMLElement>(null);
  const [bar, setBar] = useState<{ left: number; width: number } | null>(null);

  useIsoLayoutEffect(() => {
    const el = nav.current;
    if (!el) return;
    const measure = () => {
      const target = el.querySelector<HTMLElement>('[data-active="true"] [data-label]');
      if (!target) return setBar(null);
      const a = target.getBoundingClientRect();
      const b = el.getBoundingClientRect();
      setBar({ left: a.left - b.left + el.scrollLeft, width: a.width });
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [active, tabs]);

  return (
    <nav ref={nav} className={cn("relative -mb-px flex items-stretch", className)} aria-label={ariaLabel}>
      {back && (
        <Link
          href={back.href}
          className="group relative mr-2 flex h-11 items-center px-1 text-sm2 text-muted-foreground outline-none transition-colors hover:text-foreground"
        >
          <span className="flex h-7 items-center gap-1.5 whitespace-nowrap px-2 transition-colors group-hover:bg-muted/60 group-focus-visible:ring-2 group-focus-visible:ring-ring">
            <ArrowLeft className="size-4" />
            {back.label}
          </span>
          <span aria-hidden className="absolute top-1/2 -right-1 h-4 w-px -translate-y-1/2 bg-border" />
        </Link>
      )}
      {tabs.map(({ href, label, icon, count, end }) => {
        const isActive = active === label;
        return (
          <Link
            key={href}
            href={href}
            data-active={isActive}
            aria-current={isActive ? "page" : undefined}
            className={cn(
              "group relative flex h-11 items-center px-1 text-sm2 outline-none transition-colors",
              end && "ml-auto",
              isActive ? "text-foreground" : "text-muted-foreground hover:text-foreground",
            )}
          >
            <span
              data-label
              className="flex h-7 items-center gap-2 whitespace-nowrap px-2 transition-colors group-hover:bg-muted/60 group-focus-visible:ring-2 group-focus-visible:ring-ring"
            >
              {icon && (
                <span className={cn("flex transition-colors [&>svg]:size-4", isActive ? "text-primary" : "text-muted-foreground group-hover:text-foreground")}>
                  {icon}
                </span>
              )}
              <span className="relative inline-grid">
                <span aria-hidden className="invisible col-start-1 row-start-1 font-medium">{label}</span>
                <span className={cn("col-start-1 row-start-1", isActive && "font-medium")}>{label}</span>
              </span>
              {typeof count === "number" && (
                <span
                  className={cn(
                    "min-w-5 px-1.5 text-center text-micro font-medium leading-4 transition-colors",
                    isActive ? "bg-muted text-foreground" : "bg-muted/60 text-muted-foreground",
                  )}
                >
                  {count}
                </span>
              )}
            </span>
            {/* Static underline until the sliding one has a position. */}
            {isActive && bar === null && (
              <span aria-hidden className="absolute inset-x-1 bottom-0 h-0.5 bg-primary" />
            )}
          </Link>
        );
      })}
      {bar && (
        <span
          aria-hidden
          className="pointer-events-none absolute bottom-0 h-0.5 bg-primary transition-[left,width] duration-(--duration-slow)"
          style={{ left: bar.left, width: bar.width }}
        />
      )}
    </nav>
  );
}
