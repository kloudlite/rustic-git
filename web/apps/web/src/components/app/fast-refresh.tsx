"use client";

import { useEffect } from "react";
import { useRouter } from "next/navigation";

/** A faster poll for a page that is WATCHING something finish.
 *
 *  The shell's `AutoRefresh` ticks every 10 s for the whole app, which is right for a repo
 *  list and wrong for a workspace that the agent brings up in one to three seconds — the row
 *  sat on "Creating" for most of those 10 s after the disk was already mounted. A list renders
 *  this only while one of its rows is in a transitional state, so the extra timer exists exactly
 *  as long as there is something to catch and vanishes with the last "creating" row.
 *
 *  ponytail: two timers can coincide and refresh twice in one second; harmless, and cheaper than
 *  plumbing a shared scheduler through the layout. */
export function FastRefresh({ intervalMs = 2_000 }: { intervalMs?: number }) {
  const router = useRouter();
  useEffect(() => {
    const id = setInterval(() => {
      if (document.visibilityState === "visible") router.refresh();
    }, intervalMs);
    return () => clearInterval(id);
  }, [router, intervalMs]);
  return null;
}
