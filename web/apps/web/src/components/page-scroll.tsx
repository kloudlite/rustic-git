"use client";

import { usePathname } from "next/navigation";
import { useEffect, useRef } from "react";
import { ScrollArea } from "@/components/ui/scroll-area";

/** The page scrolls inside a ScrollArea, not the window, so the page's scrollbar
 *  is the same thin one every scroll box uses. Two things the window used to do
 *  for free are done here: the viewport fills the screen, and it returns to the
 *  top on navigation. */
export function PageScroll({ children }: { children: React.ReactNode }) {
  const root = useRef<HTMLDivElement>(null);
  const pathname = usePathname();

  useEffect(() => {
    // The router scrolls the window on navigation; the window no longer scrolls.
    root.current?.querySelector<HTMLElement>("[data-slot=scroll-area-viewport]")?.scrollTo({ top: 0 });
  }, [pathname]);

  return (
    <ScrollArea ref={root} type="scroll" className="h-svh" viewportClassName="[&>div]:!block">
      {children}
    </ScrollArea>
  );
}
