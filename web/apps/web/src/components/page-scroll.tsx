"use client";

import { usePathname } from "next/navigation";
import { useEffect, useRef } from "react";
import { ScrollArea } from "@/components/ui/scroll-area";

/** The page scrolls inside a ScrollArea, so its bar is the same overlay bar every
 *  scroll box uses: it takes no layout width, so nothing shifts between a short
 *  page and a long one, and no gutter has to be reserved.
 *
 *  `type="auto"` matters. With "scroll", Radix only mounts the scrollbar after a
 *  scroll event — and until it mounts the viewport stays `overflow: hidden`, so
 *  the scroll that would mount it can never happen. "auto" mounts as soon as a
 *  ResizeObserver sees the content overflow.
 *
 *  The one thing the window did for free and this does not: the router scrolls
 *  the window on navigation, which is no longer the scrolling element. */
export function PageScroll({ children }: { children: React.ReactNode }) {
  const root = useRef<HTMLDivElement>(null);
  const pathname = usePathname();

  useEffect(() => {
    root.current
      ?.querySelector<HTMLElement>("[data-radix-scroll-area-viewport]")
      ?.scrollTo({ top: 0 });
  }, [pathname]);

  return (
    <ScrollArea ref={root} type="auto" className="h-screen overflow-hidden">
      {children}
    </ScrollArea>
  );
}
